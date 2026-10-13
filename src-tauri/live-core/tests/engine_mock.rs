//! End-to-end engine tests against a mock OpenAI translation server.

mod common;

use common::*;
use live_core::config::{BilingualStrategy, LiveSettings, ScreenLanguage, TranslationMode};
use live_core::engine::{EngineConfig, LiveEngine};
use live_core::metrics::names;
use live_core::router::ScreenId;
use live_core::{CaptionHub, Lang, Metrics};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn setup(mode: TranslationMode) -> (Arc<CaptionHub>, Arc<Metrics>) {
    let mut s = LiveSettings::default();
    s.mode = mode;
    let metrics = Arc::new(Metrics::new());
    (CaptionHub::new(&s, metrics.clone()), metrics)
}

fn engine(mock: &MockOpenAi, key: &str, mode: TranslationMode, strategy: BilingualStrategy, hub: &Arc<CaptionHub>, m: &Arc<Metrics>) -> LiveEngine {
    let mut cfg = EngineConfig::new(key, mode, strategy);
    cfg.url = mock.url();
    LiveEngine::start(cfg, hub.clone(), m.clone())
}

/// Push audio every 200 ms in the background (like the real capture thread).
fn pump(engine: Arc<LiveEngine>, loud: bool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            engine.push_audio(chunk(loud));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn en_to_vi_streams_partial_captions() {
    let script = Script::default().at(
        "vi",
        1,
        vec![
            ("session.input_transcript.delta", "Good"),
            ("session.input_transcript.delta", " evening"),
            ("session.output_transcript.delta", "Chào"),
            ("session.output_transcript.delta", " buổi tối"),
        ],
    );
    let mock = MockOpenAi::start(Options { script, ..Default::default() }).await;
    let (hub, m) = setup(TranslationMode::EnToVi);
    let e = Arc::new(engine(&mock, GOOD_KEY, TranslationMode::EnToVi, BilingualStrategy::DualSession, &hub, &m));
    let p = pump(e.clone(), true);

    assert!(wait_until(3000, || hub.current_frame(ScreenId::Left).stable == "Chào buổi tối").await,
        "got {:?}", hub.current_frame(ScreenId::Left));
    // Partial (unpunctuated, unfinished) sentence is shown immediately.
    assert_eq!(hub.current_frame(ScreenId::Right).lang, Some(Lang::Vi));
    assert_eq!(e.status().state, "live");
    {
        let log = mock.log.lock().unwrap();
        let upd = &log.session_updates[0];
        assert_eq!(upd["session"]["audio"]["output"]["language"], "vi");
        assert_eq!(upd["session"]["audio"]["input"]["transcription"]["model"], "gpt-realtime-whisper");
        // 200 ms of 24 kHz PCM16 = 9600 bytes per append.
        assert!(log.appends.iter().all(|(_, n)| *n == 9600));
    }
    assert_eq!(mock.connections.load(Ordering::SeqCst), 1);
    p.abort();
    e.stop().await;
    assert!(wait_until(1000, || !mock.log.lock().unwrap().closes.is_empty()).await, "graceful session.close sent on stop");
    assert!(m.summary(names::AUDIO_BUFFERING).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vi_to_en() {
    let script = Script::default().at(
        "en",
        1,
        vec![("session.input_transcript.delta", "Xin chào"), ("session.output_transcript.delta", "Hello everyone.")],
    );
    let mock = MockOpenAi::start(Options { script, ..Default::default() }).await;
    let (hub, m) = setup(TranslationMode::ViToEn);
    let e = Arc::new(engine(&mock, GOOD_KEY, TranslationMode::ViToEn, BilingualStrategy::DualSession, &hub, &m));
    let p = pump(e.clone(), true);
    assert!(wait_until(3000, || hub.current_frame(ScreenId::Left).stable == "Hello everyone.").await);
    assert_eq!(hub.current_frame(ScreenId::Left).lang, Some(Lang::En));
    p.abort();
    e.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_dual_session_follows_language_switch() {
    // Two prewarmed sessions; vi-session carries the source transcript.
    let script = Script::default()
        .at("vi", 1, vec![
            ("session.input_transcript.delta", "Thank you all for coming tonight. "),
            ("session.output_transcript.delta", "Cảm ơn tất cả mọi người đã đến tối nay. "),
        ])
        .at("en", 1, vec![("session.output_transcript.delta", "Thank you all for coming tonight. ")])
        .at("vi", 4, vec![
            ("session.input_transcript.delta", "Xin chào quý vị đại biểu "),
            ("session.output_transcript.delta", "Xin chào quý vị đại biểu "),
        ])
        .at("en", 4, vec![("session.output_transcript.delta", "Hello distinguished delegates ")]);
    let mock = MockOpenAi::start(Options { script, ..Default::default() }).await;
    let (hub, m) = setup(TranslationMode::Auto);
    let mut s = LiveSettings::default();
    s.mode = TranslationMode::Auto;
    s.right.language = ScreenLanguage::Fixed(Lang::En);
    hub.apply_settings(&s);
    let e = Arc::new(engine(&mock, GOOD_KEY, TranslationMode::Auto, BilingualStrategy::DualSession, &hub, &m));
    let p = pump(e.clone(), true);

    // Phase 1 (English spoken): left (mode A) shows Vietnamese, right (mode B, EN) shows the original.
    assert!(wait_until(3000, || hub.current_frame(ScreenId::Left).stable.contains("Cảm ơn")).await);
    assert!(wait_until(1000, || hub.current_frame(ScreenId::Right).stable.contains("Thank you all")).await);
    // Phase 2 (Vietnamese spoken): left switches to English translation.
    assert!(wait_until(4000, || hub.current_frame(ScreenId::Left).stable == "Hello distinguished delegates").await,
        "left = {:?}", hub.current_frame(ScreenId::Left));
    assert_eq!(hub.current_frame(ScreenId::Left).lang, Some(Lang::En));
    assert_eq!(hub.spoken(), Lang::Vi);
    // English echo of phase 1 never doubled the right screen.
    let right = hub.current_frame(ScreenId::Right).stable.clone();
    assert_eq!(right.matches("Thank you all").count(), 1, "{right}");
    assert_eq!(mock.connections.load(Ordering::SeqCst), 2);
    let targets: Vec<String> = mock.log.lock().unwrap().session_updates.iter()
        .map(|u| u["session"]["audio"]["output"]["language"].as_str().unwrap().to_string()).collect();
    assert!(targets.contains(&"vi".into()) && targets.contains(&"en".into()));
    p.abort();
    e.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_switch_updates_output_language() {
    let script = Script::default().at("vi", 2, vec![("session.input_transcript.delta", "Xin chào các bạn thân mến ")]);
    let mock = MockOpenAi::start(Options { script, ..Default::default() }).await;
    let (hub, m) = setup(TranslationMode::Auto);
    let e = Arc::new(engine(&mock, GOOD_KEY, TranslationMode::Auto, BilingualStrategy::SingleSwitch, &hub, &m));
    let p = pump(e.clone(), true);
    assert!(wait_until(4000, || {
        mock.log.lock().unwrap().session_updates.iter()
            .any(|u| u["session"]["audio"]["output"]["language"] == "en")
    }).await, "session.update to en expected after Vietnamese speech");
    assert_eq!(mock.connections.load(Ordering::SeqCst), 1, "single-switch must use one session");
    p.abort();
    e.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_after_drop_without_duplicate_sessions() {
    let script = Script::default()
        .at_conn(0, "vi", 1, vec![("session.output_transcript.delta", "Trước. ")])
        .at_conn(1, "vi", 1, vec![("session.output_transcript.delta", "Sau khi kết nối lại")]);
    let mock = MockOpenAi::start(Options { script, drop_conn_after: Some((0, 2)), ..Default::default() }).await;
    let (hub, m) = setup(TranslationMode::EnToVi);
    let e = Arc::new(engine(&mock, GOOD_KEY, TranslationMode::EnToVi, BilingualStrategy::DualSession, &hub, &m));
    let p = pump(e.clone(), true);
    assert!(wait_until(6000, || hub.current_frame(ScreenId::Left).stable.contains("Sau khi kết nối lại")).await,
        "frame {:?} status {:?}", hub.current_frame(ScreenId::Left), e.status());
    assert_eq!(mock.connections.load(Ordering::SeqCst), 2);
    assert_eq!(mock.max_concurrent.load(Ordering::SeqCst), 1, "never two sessions for one lane");
    assert!(m.counter(names::RECONNECTS) >= 1);
    assert_eq!(e.status().lanes[0].reconnects, 1, "{:?}", e.status());
    let text = hub.current_frame(ScreenId::Left).stable.clone();
    assert_eq!(text.matches("Trước.").count(), 1, "{text}");
    p.abort();
    e.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_api_key_is_fatal_and_not_retried() {
    let mock = MockOpenAi::start(Options::default()).await;
    let (hub, m) = setup(TranslationMode::EnToVi);
    let e = Arc::new(engine(&mock, "sk-wrong", TranslationMode::EnToVi, BilingualStrategy::DualSession, &hub, &m));
    let p = pump(e.clone(), true);
    assert!(wait_until(3000, || e.status().state == "error").await, "{:?}", e.status());
    let st = e.status();
    assert!(st.fatal);
    assert!(st.message.unwrap().contains("API key"));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(mock.connections.load(Ordering::SeqCst), 1, "no retry loop on bad credentials");
    assert_eq!(hub.current_frame(ScreenId::Left).stable, "", "no technical text on screen");
    p.abort();
    e.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotation_before_expiry_keeps_order_without_duplicates() {
    // Connection 0 expires in 20 s → a standby is opened immediately (rotation
    // window is 90 s); cut-over happens at the first pause in the audio.
    let script = Script::default()
        .at_conn(0, "vi", 1, vec![("session.output_transcript.delta", "Phần đầu")])
        .on_close("vi", vec![" của câu cũ. "]);
    let mut expires = HashMap::new();
    expires.insert(0usize, 20u64);
    let mock = MockOpenAi::start(Options { script: script.clone(), expires_in: expires, ..Default::default() }).await;
    let (hub, m) = setup(TranslationMode::EnToVi);
    let e = Arc::new(engine(&mock, GOOD_KEY, TranslationMode::EnToVi, BilingualStrategy::DualSession, &hub, &m));
    // Speech first, then silence so the cut-over can happen at a pause.
    for _ in 0..3 {
        e.push_audio(chunk(true));
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(wait_until(3000, || mock.connections.load(Ordering::SeqCst) == 2).await, "standby opened");
    let p = pump(e.clone(), false);
    assert!(wait_until(5000, || m.counter(names::ROTATIONS) == 1).await);
    assert!(wait_until(3000, || !mock.log.lock().unwrap().closes.is_empty()).await, "old session closed gracefully");
    // Text flushed by the old session lands in order, exactly once.
    assert!(wait_until(3000, || hub.current_frame(ScreenId::Left).stable == "Phần đầu của câu cũ.").await,
        "{:?}", hub.current_frame(ScreenId::Left));
    // New session (conn 1) receives the audio after the cut-over.
    assert!(wait_until(2000, || mock.log.lock().unwrap().appends.iter().any(|(c, _)| *c == 1)).await);
    assert_eq!(m.counter(names::ROTATIONS), 1, "no rotation storm");
    p.abort();
    e.stop().await;
}
