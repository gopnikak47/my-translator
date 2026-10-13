//! Real OpenAI API integration test (opt-in, costs money, needs network).
//!
//!   OPENAI_API_KEY=sk-... LIVE_TEST_WAV=speech_en.wav LIVE_TEST_MODE=en_to_vi \
//!     cargo test --test real_api -- --ignored --nocapture
//!
//! LIVE_TEST_WAV: PCM16 WAV (mono or stereo, any sample rate) with speech.
//! LIVE_TEST_MODE: en_to_vi (default) | vi_to_en | auto
//! Streams the file in real time through the full engine and prints captions
//! and measured latency percentiles.

use live_core::audio::{PcmChunker, StreamResampler, OPENAI_RATE};
use live_core::config::{BilingualStrategy, LiveSettings, TranslationMode};
use live_core::engine::{EngineConfig, LiveEngine};
use live_core::metrics::names;
use live_core::router::ScreenId;
use live_core::{CaptionHub, Metrics};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn read_wav(path: &str) -> (Vec<f32>, u32) {
    let b = std::fs::read(path).expect("read LIVE_TEST_WAV");
    assert_eq!(&b[0..4], b"RIFF", "not a WAV file");
    let mut i = 12;
    let (mut rate, mut channels, mut bits) = (0u32, 1u16, 16u16);
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
        let body = &b[i + 8..(i + 8 + len).min(b.len())];
        if id == b"fmt " {
            channels = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            assert_eq!(bits, 16, "only PCM16 WAV supported");
            let ch = channels.max(1) as usize;
            let samples: Vec<f32> = body
                .chunks_exact(2 * ch)
                .map(|f| (0..ch).map(|c| i16::from_le_bytes([f[2 * c], f[2 * c + 1]]) as f32 / 32768.0).sum::<f32>() / ch as f32)
                .collect();
            return (samples, rate);
        }
        i += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs OPENAI_API_KEY, LIVE_TEST_WAV and network; incurs API cost"]
async fn real_openai_translation_stream() {
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
    let wav = std::env::var("LIVE_TEST_WAV").expect("LIVE_TEST_WAV");
    let mode = match std::env::var("LIVE_TEST_MODE").as_deref() {
        Ok("vi_to_en") => TranslationMode::ViToEn,
        Ok("auto") => TranslationMode::Auto,
        _ => TranslationMode::EnToVi,
    };
    let (samples, rate) = read_wav(&wav);
    let mut settings = LiveSettings::default();
    settings.mode = mode;
    let metrics = Arc::new(Metrics::new());
    let hub = CaptionHub::new(&settings, metrics.clone());
    tokio::spawn(CaptionHub::run_ticker(Arc::downgrade(&hub)));
    let mut segs = hub.subscribe_segments();
    let engine = LiveEngine::start(EngineConfig::new(key, mode, BilingualStrategy::DualSession), hub.clone(), metrics.clone());

    // Wait for the session(s) to be ready (audio before that is dropped).
    let t0 = Instant::now();
    while engine.status().state != "live" {
        assert!(t0.elapsed() < Duration::from_secs(15), "not ready: {:?}", engine.status());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let mut rs = StreamResampler::new(rate, OPENAI_RATE, 10).unwrap();
    let mut chunker = PcmChunker::new(OPENAI_RATE, 200);
    let block = (rate / 50) as usize; // 20 ms blocks like a sound card
    let start = Instant::now();
    let mut out = Vec::new();
    for (n, b) in samples.chunks(block).chain(std::iter::repeat_n(&[0f32; 480][..], 250)).enumerate() {
        // real-time pacing
        let due = start + Duration::from_millis(20 * n as u64);
        tokio::time::sleep_until(due.into()).await;
        out.clear();
        rs.process(b, &mut out).unwrap();
        chunker.push(&out, Instant::now(), |c| {
            engine.push_audio(c);
        });
        while let Ok(s) = segs.try_recv() {
            println!("[{:>6.1}s] {} {}: {}", start.elapsed().as_secs_f32(), s.lang.code(), s.kind, s.text);
        }
    }
    println!("final left frame: {:?}", hub.current_frame(ScreenId::Left));
    for name in [names::FIRST_DELTA, names::PROVIDER_ALIGNED, names::DELTA_INTERVAL, names::NETWORK_RTT, names::AUDIO_SEND, names::AUDIO_BUFFERING, names::CAPTION_PROCESSING] {
        if let Some(s) = metrics.summary(name) {
            println!("{name:32} n={:<4} median {:>7.1} ms  P95 {:>7.1} ms  worst {:>7.1} ms ({:?})", s.count, s.p50_ms, s.p95_ms, s.max_ms, s.basis);
        }
    }
    println!("reconnects: {}", metrics.counter(names::RECONNECTS));
    assert!(metrics.counter(names::DELTAS) > 0, "no translated text received");
    engine.stop().await;
}
