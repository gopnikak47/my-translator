//! Caption hub: the single owner of caption state, shared by every producer
//! (OpenAI sessions in Rust, Soniox via the webview) and every consumer
//! (overlay SSE clients, operator UI).
//!
//! Frames are published through `tokio::sync::watch` channels — one per screen.
//! A watch channel only keeps the latest value, so a slow overlay client simply
//! skips intermediate frames instead of building an unbounded backlog. That is
//! the bounded, non-blocking backpressure policy for the display path.

use crate::config::{CaptionConfig, LiveSettings, OverlayLayout, ScreenLanguage, TranslationMode};
use crate::lang::Lang;
use crate::metrics::{names, unix_ms, Basis, Metrics};
use crate::router::{CaptionRouter, CompletedSegment, Control, RouterStatus, ScreenFrame, ScreenId, Stream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch};

pub const TICK: Duration = Duration::from_millis(100);

pub struct CaptionHub {
    router: Mutex<CaptionRouter>,
    frames: [watch::Sender<Arc<ScreenFrame>>; 2],
    layout: watch::Sender<Arc<OverlayLayout>>,
    throttle_ms: AtomicU64,
    segments: broadcast::Sender<CompletedSegment>,
    seq: AtomicU64,
    pub metrics: Arc<Metrics>,
}

impl CaptionHub {
    pub fn new(settings: &LiveSettings, metrics: Arc<Metrics>) -> Arc<Self> {
        let mut router = CaptionRouter::new(settings.captions.clone(), settings.mode, settings.default_spoken);
        router.set_screen_language(ScreenId::Left, settings.left.language);
        router.set_screen_language(ScreenId::Right, settings.right.language);
        let (segments, _) = broadcast::channel(256);
        let hub = Arc::new(Self {
            router: Mutex::new(router),
            frames: [
                watch::channel(Arc::new(ScreenFrame::empty(ScreenId::Left))).0,
                watch::channel(Arc::new(ScreenFrame::empty(ScreenId::Right))).0,
            ],
            layout: watch::channel(Arc::new(settings.layout())).0,
            throttle_ms: AtomicU64::new(settings.captions.throttle_ms),
            segments,
            seq: AtomicU64::new(0),
            metrics,
        });
        {
            let mut r = hub.router.lock().unwrap();
            hub.publish(&mut r, None);
        }
        hub
    }

    /// Apply new settings (layout, caption behaviour, screen languages, mode).
    pub fn apply_settings(&self, settings: &LiveSettings) {
        let now = Instant::now();
        {
            let mut r = self.router.lock().unwrap();
            r.set_config(settings.captions.clone());
            r.set_screen_language(ScreenId::Left, settings.left.language);
            r.set_screen_language(ScreenId::Right, settings.right.language);
            if r.mode() != settings.mode {
                r.set_mode(settings.mode, settings.default_spoken, now);
            }
            self.publish(&mut r, None);
        }
        self.throttle_ms.store(settings.captions.throttle_ms, Ordering::Relaxed);
        let layout = Arc::new(settings.layout());
        self.layout.send_if_modified(|cur| {
            if **cur != *layout {
                *cur = layout;
                true
            } else {
                false
            }
        });
    }

    pub fn subscribe_screen(&self, screen: ScreenId) -> watch::Receiver<Arc<ScreenFrame>> {
        self.frames[screen.index()].subscribe()
    }

    pub fn subscribe_layout(&self) -> watch::Receiver<Arc<OverlayLayout>> {
        self.layout.subscribe()
    }

    pub fn subscribe_segments(&self) -> broadcast::Receiver<CompletedSegment> {
        self.segments.subscribe()
    }

    pub fn current_frame(&self, screen: ScreenId) -> Arc<ScreenFrame> {
        self.frames[screen.index()].borrow().clone()
    }

    pub fn throttle(&self) -> Duration {
        Duration::from_millis(self.throttle_ms.load(Ordering::Relaxed))
    }

    pub fn status(&self) -> RouterStatus {
        self.router.lock().unwrap().status()
    }

    pub fn spoken(&self) -> Lang {
        self.router.lock().unwrap().spoken()
    }

    pub fn mode(&self) -> TranslationMode {
        self.router.lock().unwrap().mode()
    }

    // ── producers ───────────────────────────────────────────────

    /// Source transcript delta. Returns the new spoken language on a switch.
    pub fn source_delta(&self, text: &str, rx_at: Instant) -> Option<Lang> {
        let mut r = self.router.lock().unwrap();
        let switched = r.source_delta(text, Instant::now());
        self.publish(&mut r, Some(rx_at));
        switched
    }

    pub fn translation_delta(&self, target: Lang, text: &str, rx_at: Instant) {
        let mut r = self.router.lock().unwrap();
        r.translation_delta(target, text, Instant::now());
        self.publish(&mut r, Some(rx_at));
    }

    pub fn translation_done(&self, target: Lang, text: &str) {
        let mut r = self.router.lock().unwrap();
        r.translation_done(target, text, Instant::now());
        self.publish(&mut r, None);
    }

    pub fn ingest_tokens(&self, stream: Stream, lang: Option<Lang>, stable: &str, tentative: &str, end: bool) {
        let rx_at = Instant::now();
        let mut r = self.router.lock().unwrap();
        r.ingest_tokens(stream, lang, stable, tentative, end, rx_at);
        self.publish(&mut r, Some(rx_at));
    }

    pub fn end_all_segments(&self) {
        let mut r = self.router.lock().unwrap();
        r.end_all_segments(Instant::now());
        self.publish(&mut r, None);
    }

    /// Returns the new spoken language if the override changed it.
    pub fn set_override(&self, lang: Option<Lang>) -> Option<Lang> {
        let mut r = self.router.lock().unwrap();
        let changed = r.set_override(lang, Instant::now());
        self.publish(&mut r, None);
        changed
    }

    pub fn set_mode(&self, mode: TranslationMode, default_spoken: Lang) {
        let mut r = self.router.lock().unwrap();
        r.set_mode(mode, default_spoken, Instant::now());
        self.publish(&mut r, None);
    }

    pub fn set_screen_language(&self, screen: ScreenId, language: ScreenLanguage) {
        let mut r = self.router.lock().unwrap();
        r.set_screen_language(screen, language);
        self.publish(&mut r, None);
    }

    pub fn set_caption_config(&self, cfg: CaptionConfig) {
        self.throttle_ms.store(cfg.throttle_ms, Ordering::Relaxed);
        let mut r = self.router.lock().unwrap();
        r.set_config(cfg);
        self.publish(&mut r, None);
    }

    pub fn control(&self, c: Control) {
        let mut r = self.router.lock().unwrap();
        r.control(c, Instant::now());
        // Hide/show must reach the screens even while frozen.
        let force = matches!(c, Control::Hide | Control::Show | Control::ToggleHidden);
        self.publish_inner(&mut r, None, force);
    }

    pub fn tick(&self) {
        let mut r = self.router.lock().unwrap();
        r.tick(Instant::now());
        let dropped = r.take_echo_dropped();
        if dropped > 0 {
            self.metrics.incr(names::ECHO_DROPPED, dropped);
        }
        self.publish(&mut r, None);
    }

    /// Spawn-able housekeeping loop; ends when the hub is dropped.
    pub async fn run_ticker(hub: Weak<CaptionHub>) {
        let mut iv = tokio::time::interval(TICK);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            match hub.upgrade() {
                Some(h) => h.tick(),
                None => break,
            }
        }
    }

    fn publish(&self, r: &mut CaptionRouter, rx_at: Option<Instant>) {
        self.publish_inner(r, rx_at, false)
    }

    fn publish_inner(&self, r: &mut CaptionRouter, rx_at: Option<Instant>, force: bool) {
        for seg in r.take_completed() {
            let _ = self.segments.send(seg);
        }
        if r.frozen && !force {
            return;
        }
        let frozen = r.frozen;
        let mut sent = false;
        for s in ScreenId::ALL {
            let mut frame = r.render(s);
            let tx = &self.frames[s.index()];
            if frozen {
                // While frozen only the visibility flag may change.
                let cur = tx.borrow().clone();
                let mut f = (*cur).clone();
                f.visible = frame.visible;
                frame = f;
            }
            let changed = !tx.borrow().same_content(&frame);
            if changed {
                frame.seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
                frame.t_pub_ms = unix_ms();
                tx.send_replace(Arc::new(frame));
                sent = true;
            }
        }
        if sent {
            if let Some(rx) = rx_at {
                self.metrics.record(
                    names::CAPTION_PROCESSING,
                    Basis::Measured,
                    rx.elapsed().as_secs_f64() * 1000.0,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> Arc<CaptionHub> {
        let mut s = LiveSettings::default();
        s.mode = TranslationMode::EnToVi;
        CaptionHub::new(&s, Arc::new(Metrics::new()))
    }

    #[test]
    fn publishes_only_on_change() {
        let h = hub();
        let mut rx = h.subscribe_screen(ScreenId::Left);
        rx.borrow_and_update();
        h.translation_delta(Lang::Vi, "Xin chào", Instant::now());
        assert!(rx.has_changed().unwrap());
        let f = rx.borrow_and_update().clone();
        assert_eq!(f.stable, "Xin chào");
        assert!(f.seq > 0 && f.t_pub_ms > 0);
        // Same content again: no new frame.
        h.tick();
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn freeze_holds_frame_but_hide_still_applies() {
        let h = hub();
        h.translation_delta(Lang::Vi, "Một", Instant::now());
        h.control(Control::Freeze);
        h.translation_delta(Lang::Vi, " hai", Instant::now());
        assert_eq!(h.current_frame(ScreenId::Left).stable, "Một");
        h.control(Control::Hide);
        let f = h.current_frame(ScreenId::Left);
        assert!(!f.visible);
        assert_eq!(f.stable, "Một");
        h.control(Control::Show);
        h.control(Control::Unfreeze);
        assert_eq!(h.current_frame(ScreenId::Left).stable, "Một hai");
    }

    #[test]
    fn completed_segments_are_broadcast() {
        let h = hub();
        let mut rx = h.subscribe_segments();
        h.translation_delta(Lang::Vi, "Câu một. Câu", Instant::now());
        let seg = rx.try_recv().unwrap();
        assert_eq!(seg.text, "Câu một.");
        assert_eq!(seg.kind, "translation");
    }

    #[test]
    fn layout_updates_only_when_changed() {
        let h = hub();
        let mut rx = h.subscribe_layout();
        rx.borrow_and_update();
        let mut s = LiveSettings::default();
        s.mode = TranslationMode::EnToVi;
        h.apply_settings(&s);
        assert!(!rx.has_changed().unwrap());
        s.left.font_size = 99;
        h.apply_settings(&s);
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow().left.font_size, 99);
    }
}
