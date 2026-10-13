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
