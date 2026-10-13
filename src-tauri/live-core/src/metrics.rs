//! Lightweight latency/counter metrics with rolling percentiles.
//!
//! Every stage is measured separately so the operator can see where time goes
//! instead of attributing everything to the AI model. Each metric records how
//! it was obtained (`Measured` from local timestamps, or `Estimated` when it
//! depends on assumptions such as speech-onset detection or provider timing
//! metadata). True mouth-to-LED latency needs external A/V sync equipment and
//! is never claimed here.

use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const WINDOW: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    Measured,
    Estimated,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Summary {
    pub basis: Basis,
    pub count: u64,
    pub last_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug)]
struct Series {
    basis: Basis,
    values: VecDeque<f64>,
    total: u64,
    max_all: f64,
}

impl Series {
    fn summary(&self) -> Summary {
        let mut v: Vec<f64> = self.values.iter().copied().collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Summary {
            basis: self.basis,
            count: self.total,
            last_ms: self.values.back().copied().unwrap_or(0.0),
            p50_ms: percentile(&v, 0.50),
            p95_ms: percentile(&v, 0.95),
            max_ms: self.max_all,
        }
    }
}

/// Nearest-rank percentile of sorted values.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[derive(Default)]
pub struct Metrics {
    series: Mutex<BTreeMap<&'static str, Series>>,
    counters: Mutex<BTreeMap<&'static str, u64>>,
    pub overlay_clients: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, name: &'static str, basis: Basis, ms: f64) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        let mut s = self.series.lock().unwrap();
        let e = s.entry(name).or_insert_with(|| Series {
            basis,
            values: VecDeque::with_capacity(WINDOW),
            total: 0,
            max_all: 0.0,
        });
        if e.values.len() == WINDOW {
            e.values.pop_front();
        }
        e.values.push_back(ms);
        e.total += 1;
        if ms > e.max_all {
            e.max_all = ms;
        }
    }

    pub fn incr(&self, name: &'static str, by: u64) {
        *self.counters.lock().unwrap().entry(name).or_insert(0) += by;
    }

    pub fn counter(&self, name: &'static str) -> u64 {
        self.counters.lock().unwrap().get(name).copied().unwrap_or(0)
    }

    pub fn summary(&self, name: &str) -> Option<Summary> {
        self.series.lock().unwrap().get(name).map(|s| s.summary())
    }

    pub fn reset(&self) {
        self.series.lock().unwrap().clear();
        self.counters.lock().unwrap().clear();
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            series: self
                .series
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.to_string(), v.summary()))
                .collect(),
            counters: self
                .counters
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
            overlay_clients: self.overlay_clients.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub series: BTreeMap<String, Summary>,
    pub counters: BTreeMap<String, u64>,
    pub overlay_clients: u64,
}

/// Wall-clock milliseconds since the Unix epoch. Used only for values that must
/// be compared with a browser's `Date.now()` on the same machine.
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Metric names (kept in one place so the UI and docs stay in sync).
pub mod names {
    /// Device capture timestamp → our audio callback (cpal timestamps).
    pub const AUDIO_CAPTURE: &str = "audio_capture_ms";
    /// Oldest sample in a sent chunk → handed to the WebSocket (chunking + queue).
    pub const AUDIO_BUFFERING: &str = "audio_buffering_ms";
    /// Time spent in WebSocket send for one audio append.
    pub const AUDIO_SEND: &str = "audio_send_ms";
    /// WebSocket ping → pong round trip to the provider.
    pub const NETWORK_RTT: &str = "network_rtt_ms";
    /// Detected speech onset → first translated delta of that utterance.
    pub const FIRST_DELTA: &str = "time_to_first_delta_ms";
    /// Interval between consecutive translated deltas while speaking.
    pub const DELTA_INTERVAL: &str = "delta_interval_ms";
    /// Wall-clock now − capture time of the audio at the delta's `elapsed_ms`.
    pub const PROVIDER_ALIGNED: &str = "provider_aligned_latency_ms";
    /// Delta received from the socket → caption frame published.
    pub const CAPTION_PROCESSING: &str = "caption_processing_ms";
    /// Frame published → received by overlay page (reported by the page).
    pub const OVERLAY_TRANSPORT: &str = "overlay_transport_ms";
    /// Frame published → painted by overlay page (reported by the page).
    pub const OVERLAY_RENDER: &str = "overlay_render_ms";
    /// Speech onset → first caption painted on an overlay (sum of the chain).
    pub const END_TO_END: &str = "end_to_end_estimate_ms";

    pub const RECONNECTS: &str = "reconnects";
    pub const ROTATIONS: &str = "session_rotations";
    pub const AUDIO_DROPPED: &str = "audio_chunks_dropped";
    pub const ECHO_DROPPED: &str = "echo_chars_dropped";
    pub const SILENCE_FILLED_MS: &str = "silence_filled_ms";
    pub const DELTAS: &str = "translation_deltas";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let m = Metrics::new();
        for i in 1..=100 {
            m.record("x", Basis::Measured, i as f64);
        }
        let s = m.summary("x").unwrap();
        assert_eq!(s.p50_ms, 50.0);
        assert_eq!(s.p95_ms, 95.0);
        assert_eq!(s.max_ms, 100.0);
        assert_eq!(s.count, 100);
    }

    #[test]
    fn window_is_bounded_but_max_is_global() {
        let m = Metrics::new();
        m.record("x", Basis::Measured, 10_000.0);
        for _ in 0..(WINDOW * 2) {
            m.record("x", Basis::Measured, 1.0);
        }
        let s = m.summary("x").unwrap();
        assert_eq!(s.p95_ms, 1.0);
        assert_eq!(s.max_ms, 10_000.0);
    }

    #[test]
    fn ignores_invalid() {
        let m = Metrics::new();
        m.record("x", Basis::Measured, f64::NAN);
        m.record("x", Basis::Measured, -1.0);
        assert!(m.summary("x").is_none());
    }

    #[test]
    fn counters() {
        let m = Metrics::new();
        m.incr("a", 2);
        m.incr("a", 3);
        assert_eq!(m.counter("a"), 5);
        assert_eq!(m.snapshot().counters["a"], 5);
    }
}
