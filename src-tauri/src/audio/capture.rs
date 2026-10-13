//! Single audio capture, fanned out to every consumer.
//!
//! Consumers:
//! * **legacy** — the webview engines (Soniox, Qwen, OpenAI desktop mode, local
//!   MLX) via a Tauri channel: 16 kHz PCM16, batched every 100 ms and sent as a
//!   raw binary IPC payload (previously a JSON array of numbers every 200 ms).
//! * **live** — the OpenAI live-event engine, entirely in Rust: 24 kHz PCM16
//!   chunks straight from the device rate (one band-limited resampling step).
//!
//! Both consumers can be attached at once and share the same device capture.
//! The dispatcher thread does all resampling; queues are bounded and never
//! block the audio callback. While the live consumer is attached and the
//! device delivers nothing (unplugged, driver reset), digital silence is fed
//! to keep the translation session's audio clock continuous and warm.

use super::microphone::{AudioStats, Frame, MicCapture};
use super::SystemAudioCapture;
use live_core::audio::{Chunk, PcmChunker, StreamResampler, LEGACY_RATE, OPENAI_RATE};
use live_core::metrics::{names, Metrics};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::ipc::{Channel, InvokeResponseBody};

const FRAME_QUEUE: usize = 256;
const LEGACY_BATCH: Duration = Duration::from_millis(100);
const STALL_FILL_AFTER: Duration = Duration::from_millis(300);
const FILL_STEP_MS: u64 = 20;

pub type LiveSink = Arc<dyn Fn(Chunk) + Send + Sync>;

#[derive(Default)]
struct Sinks {
    legacy: Option<Channel<InvokeResponseBody>>,
    live: Option<(LiveSink, u32)>,
}

struct Running {
    source: String,
    device: Option<String>,
    stop: Arc<AtomicBool>,
    mic: Option<MicCapture>,
    system: bool,
    dispatcher: Option<std::thread::JoinHandle<()>>,
}

pub struct AudioCapture {
    system: Mutex<SystemAudioCapture>,
    running: Mutex<Option<Running>>,
    sinks: Arc<Mutex<Sinks>>,
    pub stats: Arc<AudioStats>,
    metrics: Arc<Metrics>,
}

impl AudioCapture {
    pub fn new(metrics: Arc<Metrics>) -> Self {
        Self {
            system: Mutex::new(SystemAudioCapture::new()),
            running: Mutex::new(None),
            sinks: Arc::new(Mutex::new(Sinks::default())),
            stats: Arc::new(AudioStats::default()),
            metrics,
        }
    }

    fn norm(device: Option<String>) -> Option<String> {
        device.filter(|d| !d.trim().is_empty())
    }

    /// Ensure capture is running for `source`/`device`. If another consumer is
    /// attached to a different input, refuse rather than steal it.
    fn ensure(&self, source: &str, device: Option<String>, other_attached: bool) -> Result<(), String> {
        let device = Self::norm(device);
        let mut running = self.running.lock().unwrap();
        if let Some(r) = running.as_ref() {
            if r.source == source && (source == "system" || r.device == device) {
                return Ok(());
            }
            if other_attached {
                return Err(format!(
                    "The audio input is already in use ({}{}). Stop the other session first.",
                    r.source,
                    r.device.as_deref().map(|d| format!(": {d}")).unwrap_or_default()
                ));
            }
        }
        if let Some(r) = running.take() {
            self.stop_running(r);
        }
        *running = Some(self.start_running(source, device)?);
        Ok(())
    }

    fn start_running(&self, source: &str, device: Option<String>) -> Result<Running, String> {
        let (tx, rx) = mpsc::sync_channel::<Frame>(FRAME_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let mut mic = None;
        let mut system = false;
        if source == "microphone" || source == "both" {
            mic = Some(MicCapture::start(device.clone(), tx.clone(), self.stats.clone(), self.metrics.clone())?);
        }
        if source == "system" || source == "both" {
            let sys_rx = self.system.lock().unwrap().start()?;
            system = true;
            self.stats.set_state("running", None);
            if source == "system" {
                self.stats.set_device("System audio", LEGACY_RATE);
            }
            spawn_system_adapter(sys_rx, tx.clone(), self.stats.clone());
        }
        if mic.is_none() && !system {
            return Err(format!("Unknown audio source: {source}"));
        }
        drop(tx);
        let sinks = self.sinks.clone();
        let stats = self.stats.clone();
        let metrics = self.metrics.clone();
        let stop2 = stop.clone();
        let dispatcher = std::thread::Builder::new()
            .name("audio-dispatch".into())
            .spawn(move || dispatch(rx, sinks, stats, metrics, stop2))
            .map_err(|e| e.to_string())?;
        Ok(Running {
            source: source.to_string(),
            device,
            stop,
            mic,
            system,
            dispatcher: Some(dispatcher),
        })
    }

    fn stop_running(&self, mut r: Running) {
        r.stop.store(true, Ordering::SeqCst);
        if let Some(mut m) = r.mic.take() {
            m.stop();
        }
        if r.system {
            self.system.lock().unwrap().stop();
        }
        if let Some(d) = r.dispatcher.take() {
            let _ = d.join();
        }
        self.stats.set_state("stopped", None);
    }

    fn stop_if_unused(&self) {
        let unused = {
            let s = self.sinks.lock().unwrap();
            s.legacy.is_none() && s.live.is_none()
        };
        if unused {
            if let Some(r) = self.running.lock().unwrap().take() {
                self.stop_running(r);
            }
        }
    }

    /// Webview consumer (legacy `start_capture` command).
    pub fn attach_legacy(&self, source: &str, device: Option<String>, ch: Channel<InvokeResponseBody>) -> Result<(), String> {
        let live_attached = self.sinks.lock().unwrap().live.is_some();
        // The legacy path never selected a device: when live mode owns the
        // microphone, share it instead of asking for the default device.
        let device = if device.is_none() && live_attached {
            self.running.lock().unwrap().as_ref().and_then(|r| r.device.clone())
        } else {
            device
        };
        self.ensure(source, device, live_attached)?;
        self.sinks.lock().unwrap().legacy = Some(ch);
        Ok(())
    }

    pub fn detach_legacy(&self) {
        self.sinks.lock().unwrap().legacy = None;
        self.stop_if_unused();
    }

    /// Rust live engine consumer.
    pub fn attach_live(&self, source: &str, device: Option<String>, chunk_ms: u32, sink: LiveSink) -> Result<(), String> {
        let legacy_attached = self.sinks.lock().unwrap().legacy.is_some();
        self.ensure(source, device, legacy_attached)?;
        self.sinks.lock().unwrap().live = Some((sink, chunk_ms));
        Ok(())
    }

    pub fn detach_live(&self) {
        self.sinks.lock().unwrap().live = None;
        self.stop_if_unused();
    }

    pub fn stop_all(&self) {
        {
            let mut s = self.sinks.lock().unwrap();
            s.legacy = None;
            s.live = None;
        }
        self.stop_if_unused();
    }
}

fn spawn_system_adapter(rx: mpsc::Receiver<Vec<u8>>, tx: SyncSender<Frame>, stats: Arc<AudioStats>) {
    std::thread::spawn(move || {
        while let Ok(bytes) = rx.recv() {
            let samples: Vec<f32> = bytes
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                .collect();
            let lvl = live_core::audio::measure(&samples);
            stats.record_level(lvl.peak, lvl.rms, lvl.clipped);
            let dur = Duration::from_secs_f64(samples.len() as f64 / LEGACY_RATE as f64);
            let at = Instant::now().checked_sub(dur).unwrap_or_else(Instant::now);
            match tx.try_send(Frame { samples, rate: LEGACY_RATE, at, source: 1 }) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
    });
}

/// Per-source conversion state (a source = mic or system audio).
struct SourceState {
    rate: u32,
    to16: StreamResampler,
    to24: StreamResampler,
}

impl SourceState {
    fn new(rate: u32) -> Option<Self> {
        Some(Self {
            rate,
            to16: StreamResampler::new(rate, LEGACY_RATE, 10).ok()?,
            to24: StreamResampler::new(rate, OPENAI_RATE, 10).ok()?,
        })
    }
}

fn dispatch(rx: mpsc::Receiver<Frame>, sinks: Arc<Mutex<Sinks>>, stats: Arc<AudioStats>, metrics: Arc<Metrics>, stop: Arc<AtomicBool>) {
    let mut sources: [Option<SourceState>; 2] = [None, None];
    let mut legacy_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut legacy_since = Instant::now();
    let mut chunker: Option<PcmChunker> = None;
    let mut out16 = Vec::with_capacity(4096);
    let mut out24 = Vec::with_capacity(4096);
    let mut last_frame = Instant::now();
    let mut filled_until: Option<Instant> = None;

    while !stop.load(Ordering::SeqCst) {
        let frame = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(f) => Some(f),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let (legacy, live) = {
            let s = sinks.lock().unwrap();
            (s.legacy.clone(), s.live.clone())
        };

        // Live chunker follows the configured chunk length.
        match (&live, &chunker) {
            (Some((_, ms)), Some(c)) if c.chunk_samples() as u64 != OPENAI_RATE as u64 * *ms as u64 / 1000 => {
                chunker = Some(PcmChunker::new(OPENAI_RATE, *ms));
            }
            (Some((_, ms)), None) => chunker = Some(PcmChunker::new(OPENAI_RATE, *ms)),
            (None, Some(_)) => chunker = None,
            _ => {}
        }

        match frame {
            Some(f) if !f.samples.is_empty() => {
                last_frame = Instant::now();
                filled_until = None;
                let idx = (f.source as usize).min(1);
                if sources[idx].as_ref().map(|s| s.rate != f.rate).unwrap_or(true) {
                    sources[idx] = SourceState::new(f.rate);
                    if let Some(c) = chunker.as_mut() {
                        c.reset();
                    }
                }
                let Some(src) = sources[idx].as_mut() else { continue };
                if legacy.is_some() {
                    out16.clear();
                    if src.to16.process(&f.samples, &mut out16).is_ok() {
                        for &s in &out16 {
                            legacy_buf.extend_from_slice(&live_core::audio::f32_to_i16(s).to_le_bytes());
                        }
                    }
                }
                if let (Some((sink, _)), Some(c)) = (&live, chunker.as_mut()) {
                    out24.clear();
                    if src.to24.process(&f.samples, &mut out24).is_ok() {
                        c.push(&out24, f.at, |chunk| sink(chunk));
                    }
                }
            }
            Some(_) => {}
            None => {
                // Device silent/unplugged: keep the live session's clock running.
                if let (Some((sink, _)), Some(c)) = (&live, chunker.as_mut()) {
                    let now = Instant::now();
                    if now.duration_since(last_frame) >= STALL_FILL_AFTER {
                        let from = filled_until.unwrap_or(last_frame);
                        let gap = now.duration_since(from);
                        let steps = gap.as_millis() as u64 / FILL_STEP_MS;
                        if steps > 0 {
                            let n = (OPENAI_RATE as u64 * FILL_STEP_MS / 1000 * steps) as usize;
                            let zeros = vec![0f32; n];
                            c.push(&zeros, from, |chunk| sink(chunk));
                            filled_until = Some(from + Duration::from_millis(steps * FILL_STEP_MS));
                            stats.silence_filled_ms.fetch_add(steps * FILL_STEP_MS, Ordering::Relaxed);
                            metrics.incr(names::SILENCE_FILLED_MS, steps * FILL_STEP_MS);
                        }
                    }
                }
            }
        }

        if let Some(ch) = legacy.as_ref() {
            if !legacy_buf.is_empty() && legacy_since.elapsed() >= LEGACY_BATCH {
                let bytes = std::mem::replace(&mut legacy_buf, Vec::with_capacity(4096));
                if ch.send(InvokeResponseBody::Raw(bytes)).is_err() {
                    sinks.lock().unwrap().legacy = None;
                }
                legacy_since = Instant::now();
            }
        } else {
            legacy_buf.clear();
            legacy_since = Instant::now();
        }
    }
}
