//! Microphone / audio-interface capture (cpal).
//!
//! * Any input device can be selected by name (USB audio interfaces fed by the
//!   stage mixer); empty name = system default input.
//! * The cpal stream is created, owned and dropped on one dedicated thread
//!   (cpal streams are `!Send`; WASAPI also wants COM objects kept on the
//!   thread that created them). That thread also supervises the stream: when
//!   the device reports an error or stops delivering audio it is reopened
//!   with backoff — e.g. after a USB interface is unplugged and plugged back.
//! * The real-time callback reduces to mono (louder channel by default — some
//!   receivers carry the voice on one channel only), applies the software
//!   gain, updates level meters
//!   and does a non-blocking `try_send`. Resampling happens on the dispatcher
//!   thread (see `capture.rs`), never in the audio callback.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use live_core::audio::{measure, ChannelMode, ChannelPicker, I32Decoder};
use live_core::metrics::{names, Basis, Metrics};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One block of captured audio, mono f32 at the device rate.
pub struct Frame {
    pub samples: Vec<f32>,
    pub rate: u32,
    /// Estimated capture time of the first sample.
    pub at: Instant,
    /// 0 = microphone, 1 = system audio.
    pub source: u8,
}

/// Lock-free capture statistics shared with the UI.
#[derive(Default)]
pub struct AudioStats {
    peak_bits: AtomicU32,
    rms_bits: AtomicU32,
    pub clipped_total: AtomicU64,
    pub last_clip_unix_ms: AtomicU64,
    pub last_frame_unix_ms: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub device_reconnects: AtomicU64,
    pub silence_filled_ms: AtomicU64,
    /// Errors reported by the audio driver (non-fatal ones included).
    pub stream_errors: AtomicU64,
    /// Software input gain in tenths of a dB (0 = unity).
    pub gain_db_x10: AtomicI32,
    /// `ChannelMode::as_u8` (0 = auto).
    pub channel_mode: AtomicU8,
    /// Per-channel peak hold (L, R) since the last status read.
    ch_peak_bits: [AtomicU32; 2],
    /// I32 input detected as right-aligned 24-bit.
    pub i32_right_aligned: AtomicBool,
    /// (sample format, channels) of the open device.
    format: Mutex<(String, u16)>,
    state: Mutex<(String, Option<String>, String, u32)>, // (state, error, device, rate)
}

#[derive(Serialize, Clone)]
pub struct AudioStatusView {
    pub state: String,
    pub error: Option<String>,
    pub device: String,
    pub sample_rate: u32,
    /// Peak since the previous status read (0..1), and latest RMS.
    pub peak: f32,
    pub rms: f32,
    pub clipped_total: u64,
    pub clipping_recent: bool,
    pub ms_since_audio: Option<u64>,
    pub frames_dropped: u64,
    pub device_reconnects: u64,
    pub silence_filled_ms: u64,
    pub stream_errors: u64,
    pub sample_format: String,
    pub channels: u16,
    /// Per-channel peaks (L, R) before gain, since the previous read.
    pub peak_left: f32,
    pub peak_right: f32,
    pub gain_db: f32,
    pub channel_mode: String,
    pub i32_right_aligned: bool,
}

impl AudioStats {
    pub fn record_level(&self, peak: f32, rms: f32, clipped: u32) {
        // Non-negative f32 bit patterns order like the values themselves.
        self.peak_bits.fetch_max(peak.max(0.0).to_bits(), Ordering::Relaxed);
        self.rms_bits.store(rms.max(0.0).to_bits(), Ordering::Relaxed);
        let now = live_core::metrics::unix_ms();
        self.last_frame_unix_ms.store(now, Ordering::Relaxed);
        if clipped > 0 {
            self.clipped_total.fetch_add(clipped as u64, Ordering::Relaxed);
            self.last_clip_unix_ms.store(now, Ordering::Relaxed);
        }
    }

    pub fn set_state(&self, state: &str, error: Option<String>) {
        let mut s = self.state.lock().unwrap();
        s.0 = state.to_string();
        if error.is_some() || state == "running" || state == "stopped" {
            s.1 = error;
        }
    }

    pub fn set_format(&self, sample_format: &str, channels: u16) {
        *self.format.lock().unwrap() = (sample_format.to_string(), channels);
    }

    pub fn record_channel_peaks(&self, peaks: &[f32]) {
        for (i, p) in peaks.iter().take(2).enumerate() {
            self.ch_peak_bits[i].fetch_max(p.max(0.0).to_bits(), Ordering::Relaxed);
        }
    }

    /// Linear software gain.
    pub fn gain(&self) -> f32 {
        let db = self.gain_db_x10.load(Ordering::Relaxed) as f32 / 10.0;
        if db == 0.0 { 1.0 } else { 10f32.powf(db / 20.0) }
    }

    /// Set software gain (dB, clamped 0..+40) and channel mode.
    pub fn set_input_options(&self, gain_db: f32, channel_mode: ChannelMode) {
        let db = if gain_db.is_finite() { gain_db.clamp(0.0, 40.0) } else { 0.0 };
        self.gain_db_x10.store((db * 10.0).round() as i32, Ordering::Relaxed);
        self.channel_mode.store(channel_mode.as_u8(), Ordering::Relaxed);
    }

    pub fn set_device(&self, device: &str, rate: u32) {
        let mut s = self.state.lock().unwrap();
        s.2 = device.to_string();
        s.3 = rate;
    }

    /// Snapshot; resets the peak-hold.
    pub fn view(&self) -> AudioStatusView {
        let (state, error, device, rate) = self.state.lock().unwrap().clone();
        let fmt = self.format.lock().unwrap().clone();
        let now = live_core::metrics::unix_ms();
        let last = self.last_frame_unix_ms.load(Ordering::Relaxed);
        let last_clip = self.last_clip_unix_ms.load(Ordering::Relaxed);
        AudioStatusView {
            state: if state.is_empty() { "stopped".into() } else { state },
            error,
            device,
            sample_rate: rate,
            peak: f32::from_bits(self.peak_bits.swap(0, Ordering::Relaxed)),
            rms: f32::from_bits(self.rms_bits.load(Ordering::Relaxed)),
            clipped_total: self.clipped_total.load(Ordering::Relaxed),
            clipping_recent: last_clip > 0 && now.saturating_sub(last_clip) < 2000,
            ms_since_audio: if last == 0 { None } else { Some(now.saturating_sub(last)) },
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
            device_reconnects: self.device_reconnects.load(Ordering::Relaxed),
            silence_filled_ms: self.silence_filled_ms.load(Ordering::Relaxed),
            stream_errors: self.stream_errors.load(Ordering::Relaxed),
            sample_format: fmt.0,
            channels: fmt.1,
            peak_left: f32::from_bits(self.ch_peak_bits[0].swap(0, Ordering::Relaxed)),
            peak_right: f32::from_bits(self.ch_peak_bits[1].swap(0, Ordering::Relaxed)),
            gain_db: self.gain_db_x10.load(Ordering::Relaxed) as f32 / 10.0,
            channel_mode: match ChannelMode::from_u8(self.channel_mode.load(Ordering::Relaxed)) {
                ChannelMode::Auto => "auto",
                ChannelMode::Mix => "mix",
                ChannelMode::Left => "left",
                ChannelMode::Right => "right",
            }
            .into(),
            i32_right_aligned: self.i32_right_aligned.load(Ordering::Relaxed),
        }
    }
}

#[derive(Serialize, Clone)]
pub struct InputDeviceInfo {
    pub name: String,
    pub is_default: bool,
    pub channels: u16,
    pub sample_rate: u32,
}

pub fn list_input_devices() -> Vec<InputDeviceInfo> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let Ok(devices) = host.input_devices() else { return Vec::new() };
    devices
        .filter_map(|d| {
            let name = d.name().ok()?;
            let cfg = d.default_input_config().ok();
            Some(InputDeviceInfo {
                is_default: Some(&name) == default_name.as_ref(),
                channels: cfg.as_ref().map(|c| c.channels()).unwrap_or(0),
                sample_rate: cfg.as_ref().map(|c| c.sample_rate().0).unwrap_or(0),
                name,
            })
        })
        .collect()
}

/// Handle to a running microphone capture thread.
pub struct MicCapture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MicCapture {
    /// Open `device` (None/empty = default) and start the supervising thread.
    /// Fails if the device cannot be opened the first time.
    pub fn start(
        device: Option<String>,
        tx: SyncSender<Frame>,
        stats: Arc<AudioStats>,
        metrics: Arc<Metrics>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (first_tx, first_rx) = mpsc::channel::<Result<(), String>>();
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("mic-capture".into())
            .spawn(move || mic_thread(device, tx, stop2, stats, metrics, first_tx))
            .map_err(|e| format!("cannot start capture thread: {e}"))?;
        match first_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                Err("Timed out opening the audio input device".into())
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for MicCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn mic_thread(
    device: Option<String>,
    tx: SyncSender<Frame>,
    stop: Arc<AtomicBool>,
    stats: Arc<AudioStats>,
    metrics: Arc<Metrics>,
    first: mpsc::Sender<Result<(), String>>,
) {
    let mut first = Some(first);
    let mut attempt: u32 = 0;
    while !stop.load(Ordering::SeqCst) {
        let failed = Arc::new(AtomicBool::new(false));
        match open_stream(device.as_deref(), tx.clone(), stats.clone(), metrics.clone(), failed.clone()) {
            Ok(stream) => {
                if let Some(f) = first.take() {
                    let _ = f.send(Ok(()));
                }
                stats.set_state("running", None);
                let opened = Instant::now();
                let mut reason = None;
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                    if failed.load(Ordering::SeqCst) {
                        reason = Some("audio device error (disconnected?)".to_string());
                        break;
                    }
                    let last = stats.last_frame_unix_ms.load(Ordering::Relaxed);
                    let silent_for = live_core::metrics::unix_ms().saturating_sub(last);
                    if opened.elapsed() > Duration::from_secs(3) && silent_for > 2000 {
                        reason = Some("audio device stopped delivering audio".to_string());
                        break;
                    }
                }
                drop(stream);
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                if opened.elapsed() > Duration::from_secs(10) {
                    attempt = 0;
                }
                stats.device_reconnects.fetch_add(1, Ordering::Relaxed);
                stats.set_state("reconnecting", reason);
            }
            Err(e) => {
                if let Some(f) = first.take() {
                    let _ = f.send(Err(e));
                    return; // initial failure is reported to the caller
                }
                stats.set_state("reconnecting", Some(e));
            }
        }
        // Backoff: 0.5 s, 1 s, 2 s … capped at 5 s, interruptible.
        let wait = Duration::from_millis((500u64 << attempt.min(4)).min(5000));
        attempt += 1;
        let until = Instant::now() + wait;
        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    stats.set_state("stopped", None);
}

fn find_device(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device, String> {
    match name.filter(|n| !n.trim().is_empty()) {
        None => host
            .default_input_device()
            .ok_or_else(|| "No microphone / audio input found. Connect the audio interface.".to_string()),
        Some(wanted) => {
            let devices = host.input_devices().map_err(|e| format!("cannot list input devices: {e}"))?;
            for d in devices {
                if d.name().map(|n| n == wanted).unwrap_or(false) {
                    return Ok(d);
                }
            }
            Err(format!("Audio input \"{wanted}\" not found (unplugged?)"))
        }
    }
}

/// Device's default input format, falling back to a supported one (prefer
/// f32, then 48 kHz) for drivers that do not report a default.
fn input_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, String> {
    match device.default_input_config() {
        Ok(c) => Ok(c),
        Err(e) => {
            let configs: Vec<_> = device.supported_input_configs().map_err(|e2| format!("{e}; {e2}"))?.collect();
            let pick = configs
                .iter()
                .find(|c| c.sample_format() == cpal::SampleFormat::F32)
                .or_else(|| configs.first())
                .copied()
                .ok_or_else(|| format!("{e}; no supported input formats"))?;
            let rate = if pick.min_sample_rate().0 <= 48_000 && pick.max_sample_rate().0 >= 48_000 {
                cpal::SampleRate(48_000)
            } else {
                pick.max_sample_rate()
            };
            Ok(pick.with_sample_rate(rate))
        }
    }
}

fn open_stream(
    device_name: Option<&str>,
    tx: SyncSender<Frame>,
    stats: Arc<AudioStats>,
    metrics: Arc<Metrics>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = find_device(&host, device_name)?;
    let name = device.name().unwrap_or_default();
    let config = input_config(&device).map_err(|e| format!("cannot read input format of \"{name}\": {e}"))?;
    let rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    stats.set_device(&name, rate);
    let stream_config = cpal::StreamConfig {
        channels: config.channels(),
        sample_rate: config.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    let err_flag = failed.clone();
    let err_stats = stats.clone();
    let stats_i32 = stats.clone();
    let err_fn = move |err: cpal::StreamError| {
        eprintln!("[mic] stream error: {err}");
        err_stats.stream_errors.fetch_add(1, Ordering::Relaxed);
        // Only a vanished device needs reopening. Other backend errors are
        // usually transient glitches (common with USB / wireless receivers);
        // reopening on them would drop audio repeatedly. A stream that really
        // stops delivering is caught by the stall watchdog instead.
        if matches!(err, cpal::StreamError::DeviceNotAvailable) {
            err_flag.store(true, Ordering::SeqCst);
        }
    };

    // Shared per-callback handling for every sample format: interleaved f32
    // in, channel selection + gain, level metering, non-blocking hand-off.
    let fmt_name = format!("{:?}", config.sample_format());
    stats.set_format(&fmt_name, channels as u16);
    let mut counter: u32 = 0;
    let mut picker = ChannelPicker::default();
    let mut deliver = move |inter: &[f32], info: &cpal::InputCallbackInfo| {
        let mode = ChannelMode::from_u8(stats.channel_mode.load(Ordering::Relaxed));
        let gain = stats.gain();
        let mut mono = Vec::with_capacity(inter.len() / channels.max(1));
        let peaks = picker.process(inter, channels, mode, gain, &mut mono);
        stats.record_channel_peaks(&peaks);
        let lvl = measure(&mono);
        stats.record_level(lvl.peak, lvl.rms, lvl.clipped);
        let ts = info.timestamp();
        let delay = ts.callback.duration_since(&ts.capture).unwrap_or_default();
        counter = counter.wrapping_add(1);
        if counter.is_multiple_of(25) {
            metrics.record(names::AUDIO_CAPTURE, Basis::Measured, delay.as_secs_f64() * 1000.0);
        }
        let at = Instant::now().checked_sub(delay).unwrap_or_else(Instant::now);
        match tx.try_send(Frame { samples: mono, rate, at, source: 0 }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    };

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], info: &cpal::InputCallbackInfo| deliver(data, info),
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => {
            let mut buf = Vec::new();
            device.build_input_stream(
                &stream_config,
                move |data: &[i16], info: &cpal::InputCallbackInfo| {
                    buf.clear();
                    buf.extend(data.iter().map(|&s| s as f32 / 32768.0));
                    deliver(&buf, info);
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I32 => {
            let mut buf = Vec::new();
            let mut decoder = I32Decoder::default();
            let aligned_flag = stats_i32.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[i32], info: &cpal::InputCallbackInfo| {
                    buf.clear();
                    decoder.decode(data, &mut buf);
                    aligned_flag.i32_right_aligned.store(decoder.right_aligned, Ordering::Relaxed);
                    deliver(&buf, info);
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let mut buf = Vec::new();
            device.build_input_stream(
                &stream_config,
                move |data: &[u16], info: &cpal::InputCallbackInfo| {
                    buf.clear();
                    buf.extend(data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0));
                    deliver(&buf, info);
                },
                err_fn,
                None,
            )
        }
        other => return Err(format!("Unsupported input sample format: {other:?}")),
    }
    .map_err(|e| format!("cannot open \"{name}\": {e}"))?;
    stream.play().map_err(|e| format!("cannot start \"{name}\": {e}"))?;
    println!("[mic] capturing from \"{name}\" at {rate} Hz, {channels} ch");
    Ok(stream)
}
