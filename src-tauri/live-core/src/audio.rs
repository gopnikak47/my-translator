//! Device-independent audio pipeline pieces:
//!
//! * [`StreamResampler`] — continuous band-limited resampling from the device
//!   rate straight to the provider rate. State carries across callbacks, so
//!   there are no per-callback discontinuities or dropped fractional samples
//!   (the legacy path resampled every callback independently with linear
//!   interpolation and no anti-aliasing, then upsampled 16 kHz → 24 kHz again).
//! * [`PcmChunker`] — groups 24 kHz PCM16 into fixed-duration chunks (the
//!   OpenAI translation API consumes 200 ms frames) and records when the oldest
//!   sample in the chunk was captured, for the buffering-latency metric.
//! * [`OnsetDetector`] — energy-based speech-onset detection used only for the
//!   *estimated* time-to-first-caption metric.

use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use std::time::{Duration, Instant};

/// Sample rate required by the OpenAI realtime translation WebSocket.
pub const OPENAI_RATE: u32 = 24_000;
/// Sample rate of the legacy pipeline (Soniox, Qwen, local MLX).
pub const LEGACY_RATE: u32 = 16_000;

pub struct StreamResampler {
    inner: Option<SincFixedIn<f32>>,
    in_rate: u32,
    out_rate: u32,
    chunk: usize,
    pending: Vec<f32>,
    out_buf: Vec<Vec<f32>>,
}

impl StreamResampler {
    /// `chunk_ms` is the processing block; it adds at most that much latency.
    pub fn new(in_rate: u32, out_rate: u32, chunk_ms: u32) -> Result<Self, String> {
        if in_rate == 0 || out_rate == 0 {
            return Err("invalid sample rate".into());
        }
        let chunk = ((in_rate as u64 * chunk_ms.max(1) as u64) / 1000).max(16) as usize;
        let inner = if in_rate == out_rate {
            None
        } else {
            let params = SincInterpolationParameters {
                sinc_len: 64,
                f_cutoff: 0.92,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 64,
                window: WindowFunction::BlackmanHarris2,
            };
            Some(
                SincFixedIn::<f32>::new(out_rate as f64 / in_rate as f64, 1.0, params, chunk, 1)
                    .map_err(|e| format!("resampler init: {e}"))?,
            )
        };
        let out_max = inner.as_ref().map(|r| r.output_frames_max()).unwrap_or(0);
        Ok(Self {
            inner,
            in_rate,
            out_rate,
            chunk,
            pending: Vec::with_capacity(chunk * 2),
            out_buf: vec![vec![0.0; out_max]],
        })
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    /// Feed mono f32 samples, append resampled output to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
        let Some(rs) = self.inner.as_mut() else {
            out.extend_from_slice(input);
            return Ok(());
        };
        self.pending.extend_from_slice(input);
        let mut offset = 0;
        while self.pending.len() - offset >= self.chunk {
            let block = &self.pending[offset..offset + self.chunk];
            let (_, produced) = rs
                .process_into_buffer(&[block], &mut self.out_buf, None)
                .map_err(|e| format!("resample: {e}"))?;
            out.extend_from_slice(&self.out_buf[0][..produced]);
            offset += self.chunk;
        }
        if offset > 0 {
            self.pending.drain(..offset);
        }
        Ok(())
    }
}

/// Mix interleaved frames down to mono.
pub fn downmix_f32(data: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels <= 1 {
        out.extend_from_slice(data);
        return;
    }
    let inv = 1.0 / channels as f32;
    out.extend(data.chunks_exact(channels).map(|f| f.iter().sum::<f32>() * inv));
}

pub fn downmix_i16(data: &[i16], channels: usize, out: &mut Vec<f32>) {
    let ch = channels.max(1);
    let inv = 1.0 / (ch as f32 * 32768.0);
    out.extend(data.chunks_exact(ch).map(|f| f.iter().map(|&s| s as f32).sum::<f32>() * inv));
}

pub fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// How a multi-channel input is reduced to mono.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    /// Follow the louder channel (with hysteresis). Robust for receivers that
    /// put the voice on one channel only, or whose channels partly cancel.
    Auto,
    /// Average all channels (classic downmix).
    Mix,
    Left,
    Right,
}

impl ChannelMode {
    pub fn from_code(code: &str) -> Self {
        match code {
            "mix" => ChannelMode::Mix,
            "left" => ChannelMode::Left,
            "right" => ChannelMode::Right,
            _ => ChannelMode::Auto,
        }
    }
    pub fn as_u8(self) -> u8 {
        match self {
            ChannelMode::Auto => 0,
            ChannelMode::Mix => 1,
            ChannelMode::Left => 2,
            ChannelMode::Right => 3,
        }
    }
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => ChannelMode::Mix,
            2 => ChannelMode::Left,
            3 => ChannelMode::Right,
            _ => ChannelMode::Auto,
        }
    }
}

/// Stateful interleaved → mono reducer.
#[derive(Debug, Default)]
pub struct ChannelPicker {
    smoothed: Vec<f32>,
    current: usize,
}

impl ChannelPicker {
    /// Reduce interleaved `data` to mono into `out`, applying `gain`.
    /// Returns the per-channel peaks of the block (before gain).
    pub fn process(&mut self, data: &[f32], channels: usize, mode: ChannelMode, gain: f32, out: &mut Vec<f32>) -> Vec<f32> {
        let ch = channels.max(1);
        let frames = data.len() / ch;
        let mut peaks = vec![0f32; ch];
        let mut power = vec![0f32; ch];
        for f in data.chunks_exact(ch) {
            for (c, &v) in f.iter().enumerate() {
                let a = v.abs();
                if a > peaks[c] {
                    peaks[c] = a;
                }
                power[c] += v * v;
            }
        }
        if ch == 1 {
            out.extend(data.iter().map(|v| v * gain));
            return peaks;
        }
        if self.smoothed.len() != ch {
            self.smoothed = vec![0.0; ch];
            self.current = 0;
        }
        let n = frames.max(1) as f32;
        for (sm, p) in self.smoothed.iter_mut().zip(&power) {
            *sm = 0.85 * *sm + 0.15 * (p / n);
        }
        let pick = match mode {
            ChannelMode::Mix => None,
            ChannelMode::Left => Some(0),
            ChannelMode::Right => Some(1.min(ch - 1)),
            ChannelMode::Auto => {
                let best = (0..ch).max_by(|&a, &b| self.smoothed[a].total_cmp(&self.smoothed[b])).unwrap_or(0);
                // Switch only when the other channel is clearly louder (≥3 dB).
                if best != self.current && self.smoothed[best] > 2.0 * self.smoothed[self.current] {
                    self.current = best;
                }
                Some(self.current)
            }
        };
        match pick {
            Some(c) => out.extend(data.chunks_exact(ch).map(|f| f[c] * gain)),
            None => {
                let k = gain / ch as f32;
                out.extend(data.chunks_exact(ch).map(|f| f.iter().sum::<f32>() * k));
            }
        }
        peaks
    }
}

/// Converts 32-bit integer PCM to f32, detecting drivers that deliver 24-bit
/// samples right-aligned in the 32-bit container (would otherwise read 48 dB
/// too quiet). Self-correcting: any value outside the 24-bit range switches
/// back to standard left-aligned scaling.
#[derive(Debug, Default)]
pub struct I32Decoder {
    pub right_aligned: bool,
    evidence_blocks: u32,
}

impl I32Decoder {
    pub fn decode(&mut self, data: &[i32], out: &mut Vec<f32>) {
        let mut max_abs: u32 = 0;
        let mut low_bits = false;
        for &s in data {
            max_abs = max_abs.max(s.unsigned_abs());
            low_bits |= s & 0xFF != 0;
        }
        if max_abs >= 1 << 23 {
            self.right_aligned = false;
            self.evidence_blocks = 0;
        } else if low_bits && max_abs >= 1 << 12 {
            // Values confined to 24 bits with populated low bits, and clearly
            // above silence: typical of right-aligned 24-bit data.
            self.evidence_blocks += 1;
            if self.evidence_blocks >= 20 {
                self.right_aligned = true;
            }
        }
        let scale = if self.right_aligned { 1.0 / 8_388_608.0 } else { 1.0 / 2_147_483_648.0 };
        out.extend(data.iter().map(|&s| (s as f32 * scale).clamp(-1.0, 1.0)));
    }
}

/// Peak / RMS / clipped-sample statistics of a block.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Level {
    pub peak: f32,
    pub rms: f32,
    pub clipped: u32,
}

pub fn measure(samples: &[f32]) -> Level {
    if samples.is_empty() {
        return Level::default();
    }
    let mut peak = 0f32;
    let mut sum = 0f64;
    let mut clipped = 0;
    for &s in samples {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        if a >= 0.999 {
            clipped += 1;
        }
        sum += (s as f64) * (s as f64);
    }
    Level {
        peak,
        rms: (sum / samples.len() as f64).sqrt() as f32,
        clipped,
    }
}

/// A finished PCM16 chunk ready to send.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub pcm: Vec<u8>,
    /// Capture time of the oldest sample in the chunk.
    pub oldest: Instant,
    pub samples: usize,
}

pub struct PcmChunker {
    rate: u32,
    chunk_samples: usize,
    buf: Vec<u8>,
    oldest: Option<Instant>,
}

impl PcmChunker {
    pub fn new(rate: u32, chunk_ms: u32) -> Self {
        let chunk_samples = ((rate as u64 * chunk_ms as u64) / 1000).max(1) as usize;
        Self {
            rate,
            chunk_samples,
            buf: Vec::with_capacity(chunk_samples * 2),
            oldest: None,
        }
    }

    pub fn chunk_samples(&self) -> usize {
        self.chunk_samples
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Append samples captured at `captured_at` (time of the FIRST sample in
    /// `samples`). Emits complete chunks through `emit`.
    pub fn push(&mut self, samples: &[f32], captured_at: Instant, mut emit: impl FnMut(Chunk)) {
        let mut t = captured_at;
        let per_sample = Duration::from_secs_f64(1.0 / self.rate as f64);
        let mut i = 0;
        while i < samples.len() {
            if self.oldest.is_none() {
                self.oldest = Some(t);
            }
            let room = self.chunk_samples - self.buf.len() / 2;
            let take = room.min(samples.len() - i);
            for &s in &samples[i..i + take] {
                self.buf.extend_from_slice(&f32_to_i16(s).to_le_bytes());
            }
            i += take;
            t += per_sample * take as u32;
            if self.buf.len() / 2 == self.chunk_samples {
                let pcm = std::mem::replace(&mut self.buf, Vec::with_capacity(self.chunk_samples * 2));
                emit(Chunk {
                    pcm,
                    oldest: self.oldest.take().unwrap_or(captured_at),
                    samples: self.chunk_samples,
                });
            }
        }
    }

    /// Discard partial data (e.g. when the source restarts).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.oldest = None;
    }
}

/// Speech onset after silence, for the time-to-first-caption estimate.
pub struct OnsetDetector {
    threshold_rms: f32,
    min_silence: Duration,
    quiet_since: Option<Instant>,
    in_speech: bool,
}

impl OnsetDetector {
    pub fn new(threshold_rms: f32, min_silence: Duration) -> Self {
        Self {
            threshold_rms,
            min_silence,
            quiet_since: None,
            in_speech: false,
        }
    }

    /// Feed one block's RMS. Returns `true` at a speech onset that followed at
    /// least `min_silence` of quiet.
    pub fn feed(&mut self, rms: f32, at: Instant) -> bool {
        if rms >= self.threshold_rms {
            let onset = !self.in_speech
                && self
                    .quiet_since
                    .map(|q| at.duration_since(q) >= self.min_silence)
                    .unwrap_or(true);
            self.in_speech = true;
            self.quiet_since = None;
            onset
        } else {
            if self.quiet_since.is_none() {
                self.quiet_since = Some(at);
            }
            if let Some(q) = self.quiet_since {
                if at.duration_since(q) >= self.min_silence {
                    self.in_speech = false;
                }
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    #[test]
    fn resample_48k_to_24k_keeps_duration_across_odd_blocks() {
        let mut rs = StreamResampler::new(48_000, 24_000, 10).unwrap();
        let input = sine(48_000, 440.0, 1.0);
        let mut out = Vec::new();
        // Feed in irregular callback sizes like a real device.
        for block in input.chunks(441) {
            rs.process(block, &mut out).unwrap();
        }
        // Within one processing block (10 ms) of the exact 24 000 samples.
        assert!((out.len() as i64 - 24_000).abs() <= 240, "{}", out.len());
        // Tone survives with roughly the same amplitude.
        let lvl = measure(&out[2000..20000]);
        assert!(lvl.peak > 0.4 && lvl.peak < 0.6, "{:?}", lvl);
    }

    #[test]
    fn resample_44100_to_24k() {
        let mut rs = StreamResampler::new(44_100, 24_000, 10).unwrap();
        let mut out = Vec::new();
        rs.process(&sine(44_100, 300.0, 2.0), &mut out).unwrap();
        assert!((out.len() as i64 - 48_000).abs() <= 480, "{}", out.len());
    }

    #[test]
    fn anti_aliasing_removes_tone_above_new_nyquist() {
        // 15 kHz at 48 kHz must not alias into the 24 kHz output.
        let mut rs = StreamResampler::new(48_000, 24_000, 10).unwrap();
        let mut out = Vec::new();
        rs.process(&sine(48_000, 15_000.0, 0.5), &mut out).unwrap();
        let lvl = measure(&out[1000..]);
        assert!(lvl.rms < 0.02, "aliased energy {:?}", lvl);
    }

    #[test]
    fn passthrough_when_rates_match() {
        let mut rs = StreamResampler::new(24_000, 24_000, 10).unwrap();
        let mut out = Vec::new();
        rs.process(&[0.1, 0.2], &mut out).unwrap();
        assert_eq!(out, vec![0.1, 0.2]);
    }

    #[test]
    fn chunker_emits_exact_200ms_chunks() {
        let mut c = PcmChunker::new(24_000, 200);
        let t0 = Instant::now();
        let mut chunks = Vec::new();
        let block = vec![0.25f32; 1000];
        let mut t = t0;
        for _ in 0..12 {
            c.push(&block, t, |ch| chunks.push(ch));
            t += Duration::from_secs_f64(1000.0 / 24_000.0);
        }
        assert_eq!(chunks.len(), 2); // 12 000 samples → 2 × 4 800
        for ch in &chunks {
            assert_eq!(ch.pcm.len(), 4800 * 2);
        }
        assert_eq!(chunks[0].oldest, t0);
        assert!(chunks[1].oldest > t0);
        let s = i16::from_le_bytes([chunks[0].pcm[0], chunks[0].pcm[1]]);
        assert_eq!(s, f32_to_i16(0.25));
    }

    #[test]
    fn downmix_and_measure() {
        let mut out = Vec::new();
        downmix_f32(&[1.0, 0.0, 0.5, 0.5], 2, &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
        out.clear();
        downmix_i16(&[i16::MAX, i16::MAX], 2, &mut out);
        assert!(out[0] > 0.99);
        let l = measure(&[1.0, -1.0, 0.0, 0.0]);
        assert_eq!(l.clipped, 2);
        assert_eq!(l.peak, 1.0);
    }

    #[test]
    fn auto_channel_follows_the_voice_channel() {
        let mut p = ChannelPicker::default();
        // Voice on the right channel only, left silent.
        let block: Vec<f32> = (0..480).flat_map(|i| [0.0, ((i as f32) * 0.1).sin() * 0.5]).collect();
        let mut out = Vec::new();
        for _ in 0..10 {
            out.clear();
            p.process(&block, 2, ChannelMode::Auto, 1.0, &mut out);
        }
        assert!(measure(&out).peak > 0.45, "auto must not halve a one-sided signal");
        out.clear();
        p.process(&block, 2, ChannelMode::Mix, 1.0, &mut out);
        assert!(measure(&out).peak < 0.26);
    }

    #[test]
    fn auto_channel_avoids_phase_cancellation_and_applies_gain() {
        let mut p = ChannelPicker::default();
        let block: Vec<f32> = (0..480).flat_map(|i| { let v = ((i as f32) * 0.1).sin() * 0.1; [v, -v] }).collect();
        let mut out = Vec::new();
        let peaks = p.process(&block, 2, ChannelMode::Auto, 4.0, &mut out);
        assert!((peaks[0] - 0.1).abs() < 0.01 && (peaks[1] - 0.1).abs() < 0.01);
        assert!(measure(&out).peak > 0.35, "gain ×4 on one channel, no cancellation");
        out.clear();
        p.process(&block, 2, ChannelMode::Mix, 1.0, &mut out);
        assert!(measure(&out).peak < 1e-6, "mix cancels out-of-phase channels");
    }

    #[test]
    fn i32_decoder_detects_right_aligned_24_bit() {
        let mut d = I32Decoder::default();
        // -10 dBFS tone as right-aligned 24-bit (max ≈ 0.32 · 2^23).
        let block: Vec<i32> = (0..480).map(|i| (((i as f32) * 0.05).sin() * 0.32 * 8_388_607.0) as i32 | 1).collect();
        let mut out = Vec::new();
        for _ in 0..25 {
            out.clear();
            d.decode(&block, &mut out);
        }
        assert!(d.right_aligned);
        assert!(measure(&out).peak > 0.3);
        // Standard left-aligned data (loud) switches back immediately.
        let loud: Vec<i32> = block.iter().map(|&v| v.wrapping_shl(8)).collect();
        out.clear();
        d.decode(&loud, &mut out);
        assert!(!d.right_aligned);
        assert!(measure(&out).peak > 0.3);
    }

    #[test]
    fn i32_decoder_keeps_left_aligned_24_in_32() {
        let mut d = I32Decoder::default();
        // Proper left-aligned 24-in-32: low byte always zero, even when quiet.
        let quiet: Vec<i32> = (0..480).map(|i| ((((i as f32) * 0.05).sin() * 0.001 * 8_388_607.0) as i32) << 8).collect();
        let mut out = Vec::new();
        for _ in 0..50 {
            out.clear();
            d.decode(&quiet, &mut out);
        }
        assert!(!d.right_aligned);
    }

    #[test]
    fn onset_requires_preceding_silence() {
        let mut d = OnsetDetector::new(0.02, Duration::from_millis(500));
        let t0 = Instant::now();
        assert!(d.feed(0.1, t0)); // first speech
        assert!(!d.feed(0.1, t0 + Duration::from_millis(100)));
        assert!(!d.feed(0.0, t0 + Duration::from_millis(200)));
        assert!(!d.feed(0.1, t0 + Duration::from_millis(300))); // short pause
        assert!(!d.feed(0.0, t0 + Duration::from_millis(400)));
        assert!(!d.feed(0.0, t0 + Duration::from_millis(1000)));
        assert!(d.feed(0.1, t0 + Duration::from_millis(1100))); // new utterance
    }
}
