//! OpenAI live-translation supervisor.
//!
//! Owns 1–2 persistent translation sessions ("lanes"), feeds them audio from a
//! single capture, and routes their transcript deltas into the [`CaptionHub`].
//!
//! Lanes by mode:
//!
//! | mode / strategy        | lanes                                   |
//! |------------------------|-----------------------------------------|
//! | EN→VI                  | →vi (+ source transcript)               |
//! | VI→EN                  | →en (+ source transcript)               |
//! | Auto, dual-session     | →vi (+ source transcript), →en          |
//! | Auto, single-switch    | →opposite(spoken) (+ source transcript) |
//!
//! Reliability:
//! * Unexpected disconnects reconnect with capped exponential backoff + jitter;
//!   each lane has at most one active session, so reconnects never duplicate.
//! * Authentication / quota errors stop retries (no runaway charges).
//! * Sessions are rotated before `expires_at` make-before-break: the new session
//!   connects first, audio is switched over at a pause (or a deadline), the old
//!   session is sent `session.close` so it flushes its remaining output, and the
//!   new session's output is held back until the old one has finished, which
//!   keeps caption order intact without duplicates.
//! * Audio is pushed with `try_send` into bounded queues; a session that cannot
//!   keep up is dropped and reconnected instead of accumulating latency.

use crate::audio::{Chunk, OnsetDetector};
use crate::config::{BilingualStrategy, TranslationMode};
use crate::hub::CaptionHub;
use crate::lang::Lang;
use crate::metrics::{names, unix_ms, Basis, Metrics};
use crate::openai::{audio_append_message, run_session, SessionCmd, SessionEvent, SessionSpec, Tagged, OPENAI_TRANSLATE_URL};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const BACKOFF_START_MS: u64 = 300;
const BACKOFF_MAX_MS: u64 = 10_000;
const HEALTHY_RESET: Duration = Duration::from_secs(30);
const ROTATE_BEFORE: Duration = Duration::from_secs(90);
const ROTATE_DEADLINE: Duration = Duration::from_secs(15);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(6);
const SESSION_QUEUE: usize = 32;
const MAX_CONSECUTIVE_DROPS: u32 = 15;
const QUIET_RMS: f32 = 0.01;
const DELTA_GAP: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub api_key: String,
    pub url: String,
    pub mode: TranslationMode,
    pub strategy: BilingualStrategy,
    pub noise_reduction: Option<String>,
    pub chunk_ms: u32,
}

impl EngineConfig {
    pub fn new(api_key: impl Into<String>, mode: TranslationMode, strategy: BilingualStrategy) -> Self {
        Self {
            api_key: api_key.into(),
            url: OPENAI_TRANSLATE_URL.to_string(),
            mode,
            strategy,
            noise_reduction: Some("near_field".into()),
            chunk_ms: 200,
        }
    }
}

#[derive(Debug)]
enum EngineCmd {
    Audio(Chunk),
    SpokenChanged(Lang),
    Stop(oneshot::Sender<()>),
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LaneStatus {
    pub target: Lang,
    pub transcription: bool,
    pub state: &'static str,
    pub reconnects: u64,
    pub expires_in_s: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EngineStatus {
    pub running: bool,
    /// "connecting" | "live" | "degraded" | "reconnecting" | "error" | "stopped"
    pub state: &'static str,
    pub lanes: Vec<LaneStatus>,
    /// Plain-language message for the operator (never shown on the LED).
    pub message: Option<String>,
    pub fatal: bool,
    pub last_delta_unix_ms: u64,
}

impl Default for EngineStatus {
    fn default() -> Self {
        Self {
            running: false,
            state: "stopped",
            lanes: Vec::new(),
            message: None,
            fatal: false,
            last_delta_unix_ms: 0,
        }
    }
}

pub struct LiveEngine {
    cmd_tx: mpsc::Sender<EngineCmd>,
    status: Arc<Mutex<EngineStatus>>,
    metrics: Arc<Metrics>,
}

impl LiveEngine {
    /// Start the supervisor. Must be called inside a Tokio runtime.
    pub fn start(cfg: EngineConfig, hub: Arc<CaptionHub>, metrics: Arc<Metrics>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let status = Arc::new(Mutex::new(EngineStatus {
            running: true,
            state: "connecting",
            ..EngineStatus::default()
        }));
        let sup = Supervisor::new(cfg, hub, metrics.clone(), status.clone());
        tokio::spawn(sup.run(cmd_rx));
        Self { cmd_tx, status, metrics }
    }

    /// Non-blocking. Returns false (and counts a drop) if the engine queue is full.
    pub fn push_audio(&self, chunk: Chunk) -> bool {
        match self.cmd_tx.try_send(EngineCmd::Audio(chunk)) {
            Ok(()) => true,
            Err(_) => {
                self.metrics.incr(names::AUDIO_DROPPED, 1);
                false
            }
        }
    }

    /// Tell the engine the spoken language changed by operator override.
    pub fn spoken_changed(&self, lang: Lang) {
        let _ = self.cmd_tx.try_send(EngineCmd::SpokenChanged(lang));
    }

    pub fn status(&self) -> EngineStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn is_running(&self) -> bool {
        !self.cmd_tx.is_closed()
    }

    /// Graceful stop: sessions get `session.close` and up to ~3 s to flush.
    pub async fn stop(&self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(EngineCmd::Stop(tx)).await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
        }
    }
}

struct Sess {
    id: u64,
    tx: mpsc::Sender<SessionCmd>,
    ready_at: Option<Instant>,
    expires_at: Option<Instant>,
    /// (audio position in ms at chunk start, capture time of chunk start)
    audio_map: VecDeque<(u64, Instant)>,
    sent_ms: u64,
    consecutive_drops: u32,
}

impl Sess {
    fn capture_time_of(&self, elapsed_ms: u64) -> Option<Instant> {
        let idx = self.audio_map.partition_point(|(start, _)| *start <= elapsed_ms);
        if idx == 0 {
            return None;
        }
        let (start, t) = self.audio_map[idx - 1];
        Some(t + Duration::from_millis(elapsed_ms - start))
    }
}

struct Lane {
    target: Lang,
    transcription: bool,
    active: Option<Sess>,
    standby: Option<Sess>,
    draining: Option<Sess>,
    cutover_at: Option<Instant>,
    /// Active-session events held until the draining session finished.
    held: Vec<SessionEvent>,
    retry_at: Option<Instant>,
    backoff_ms: u64,
    rotate_retry_at: Option<Instant>,
    fatal: Option<String>,
    reconnects: u64,
    last_error: Option<String>,
}

impl Lane {
    fn new(target: Lang, transcription: bool) -> Self {
        Self {
            target,
            transcription,
            active: None,
            standby: None,
            draining: None,
            cutover_at: None,
            held: Vec::new(),
            retry_at: Some(Instant::now()),
            backoff_ms: BACKOFF_START_MS,
            rotate_retry_at: None,
            fatal: None,
            reconnects: 0,
            last_error: None,
        }
    }
}

struct Supervisor {
    cfg: EngineConfig,
    hub: Arc<CaptionHub>,
    metrics: Arc<Metrics>,
    status: Arc<Mutex<EngineStatus>>,
    lanes: Vec<Lane>,
    next_id: u64,
    ev_tx: mpsc::Sender<Tagged>,
    ev_rx: Option<mpsc::Receiver<Tagged>>,
    quiet_chunks: u32,
    onset: OnsetDetector,
    pending_onset: Option<Instant>,
    last_delta: Option<Instant>,
    last_delta_unix_ms: u64,
    stopping: bool,
}

impl Supervisor {
    fn new(cfg: EngineConfig, hub: Arc<CaptionHub>, metrics: Arc<Metrics>, status: Arc<Mutex<EngineStatus>>) -> Self {
        let spoken = hub.spoken();
        let lanes = match (cfg.mode, cfg.strategy) {
            (TranslationMode::EnToVi, _) => vec![Lane::new(Lang::Vi, true)],
            (TranslationMode::ViToEn, _) => vec![Lane::new(Lang::En, true)],
            (TranslationMode::Auto, BilingualStrategy::DualSession) => {
                vec![Lane::new(Lang::Vi, true), Lane::new(Lang::En, false)]
            }
            (TranslationMode::Auto, BilingualStrategy::SingleSwitch) => vec![Lane::new(spoken.opposite(), true)],
        };
        let (ev_tx, ev_rx) = mpsc::channel(1024);
        Self {
            cfg,
            hub,
            metrics,
            status,
            lanes,
            next_id: 1,
            ev_tx,
            ev_rx: Some(ev_rx),
            quiet_chunks: 0,
            onset: OnsetDetector::new(0.02, Duration::from_millis(600)),
            pending_onset: None,
            last_delta: None,
            last_delta_unix_ms: 0,
            stopping: false,
        }
    }

    async fn run(mut self, mut cmd_rx: mpsc::Receiver<EngineCmd>) {
        let mut ev_rx = self.ev_rx.take().unwrap();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stop_ack: Option<oneshot::Sender<()>> = None;
        let mut stop_deadline: Option<Instant> = None;
        self.on_tick();
        loop {
            tokio::select! {
                cmd = cmd_rx.recv(), if !self.stopping => match cmd {
                    Some(EngineCmd::Audio(chunk)) => self.on_audio(chunk),
                    Some(EngineCmd::SpokenChanged(l)) => self.on_spoken_changed(l),
                    Some(EngineCmd::Stop(ack)) => {
                        self.begin_stop();
                        stop_ack = Some(ack);
                        stop_deadline = Some(Instant::now() + Duration::from_secs(3));
                    }
                    None => {
                        self.begin_stop();
                        stop_deadline = Some(Instant::now() + Duration::from_secs(3));
                    }
                },
                Some((lane, id, ev)) = ev_rx.recv() => self.on_event(lane, id, ev),
                _ = tick.tick() => {
                    if let Some(d) = stop_deadline {
                        if Instant::now() >= d || self.all_ended() {
                            break;
                        }
                    } else {
                        self.on_tick();
                    }
                }
            }
        }
        // Dropping the senders ends any session still flushing.
        for lane in self.lanes.iter_mut() {
            lane.active = None;
            lane.standby = None;
            lane.draining = None;
        }
        self.hub.end_all_segments();
        {
            let mut st = self.status.lock().unwrap();
            st.running = false;
            st.state = "stopped";
            st.message = None;
            for l in st.lanes.iter_mut() {
                l.state = "closed";
            }
        }
        if let Some(ack) = stop_ack {
            let _ = ack.send(());
        }
    }

    fn all_ended(&self) -> bool {
        self.lanes.iter().all(|l| l.active.is_none() && l.standby.is_none() && l.draining.is_none())
    }

    fn begin_stop(&mut self) {
        self.stopping = true;
        for lane in self.lanes.iter_mut() {
            lane.standby = None;
            for s in [lane.active.as_ref(), lane.draining.as_ref()].into_iter().flatten() {
                let _ = s.tx.try_send(SessionCmd::Close);
            }
        }
    }

    fn spawn_session(&mut self, lane_idx: usize) -> Sess {
        let id = self.next_id;
        self.next_id += 1;
        let lane = &self.lanes[lane_idx];
        let spec = SessionSpec {
            url: self.cfg.url.clone(),
            api_key: self.cfg.api_key.clone(),
            target: lane.target,
            transcription: lane.transcription,
            noise_reduction: self.cfg.noise_reduction.clone(),
        };
        let (tx, rx) = mpsc::channel(SESSION_QUEUE);
        tokio::spawn(run_session(lane_idx, id, spec, rx, self.ev_tx.clone(), self.metrics.clone()));
        Sess {
            id,
            tx,
            ready_at: None,
            expires_at: None,
            audio_map: VecDeque::new(),
            sent_ms: 0,
            consecutive_drops: 0,
        }
    }

    fn on_audio(&mut self, chunk: Chunk) {
        // Level analysis on 20 ms sub-blocks (onset metric + pause detection).
        let samples: Vec<i16> = chunk.pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let rate_per_ms = (chunk.samples as f64 / self.cfg.chunk_ms.max(1) as f64).max(1.0);
        let sub = (rate_per_ms * 20.0) as usize;
        let mut chunk_sum = 0f64;
        for (i, block) in samples.chunks(sub.max(1)).enumerate() {
            let sum: f64 = block.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum();
            chunk_sum += sum;
            let rms = (sum / block.len().max(1) as f64).sqrt() as f32;
            let at = chunk.oldest + Duration::from_millis((i * 20) as u64);
            if self.onset.feed(rms, at) {
                self.pending_onset = Some(at);
            }
        }
        let chunk_rms = (chunk_sum / samples.len().max(1) as f64).sqrt() as f32;
        if chunk_rms < QUIET_RMS {
            self.quiet_chunks = self.quiet_chunks.saturating_add(1);
        } else {
            self.quiet_chunks = 0;
        }

        // Rotation cut-over happens on a chunk boundary so no audio is split.
        self.maybe_cutover();

        let msg = audio_append_message(&chunk.pcm);
        let ms = self.cfg.chunk_ms;
        self.metrics.record(
            names::AUDIO_BUFFERING,
            Basis::Measured,
            chunk.oldest.elapsed().as_secs_f64() * 1000.0,
        );
        let mut overflowed = Vec::new();
        for (idx, lane) in self.lanes.iter_mut().enumerate() {
            let Some(sess) = lane.active.as_mut() else { continue };
            if sess.ready_at.is_none() {
                continue; // not configured yet: audio before session.created is dropped
            }
            match sess.tx.try_send(SessionCmd::Audio { msg: msg.clone(), ms, oldest: chunk.oldest }) {
                Ok(()) => {
                    sess.consecutive_drops = 0;
                    sess.audio_map.push_back((sess.sent_ms, chunk.oldest));
                    if sess.audio_map.len() > 3000 {
                        sess.audio_map.pop_front();
                    }
                    sess.sent_ms += ms as u64;
                }
                Err(_) => {
                    self.metrics.incr(names::AUDIO_DROPPED, 1);
                    sess.consecutive_drops += 1;
                    if sess.consecutive_drops >= MAX_CONSECUTIVE_DROPS {
                        overflowed.push(idx);
                    }
                }
            }
        }
        for idx in overflowed {
            // The session cannot keep up (stalled network): replace it rather
            // than let latency grow.
            let lane = &mut self.lanes[idx];
            lane.active = None;
            lane.last_error = Some("audio backlog — session restarted".into());
            self.schedule_retry(idx);
        }
    }

    fn on_spoken_changed(&mut self, spoken: Lang) {
        if self.cfg.mode == TranslationMode::Auto && self.cfg.strategy == BilingualStrategy::SingleSwitch {
            let target = spoken.opposite();
            if let Some(lane) = self.lanes.get_mut(0) {
                if lane.target != target {
                    lane.target = target;
                    for s in [lane.active.as_ref(), lane.standby.as_ref()].into_iter().flatten() {
                        let _ = s.tx.try_send(SessionCmd::SetTarget(target));
                    }
                }
            }
        }
    }

    fn on_event(&mut self, lane_idx: usize, id: u64, ev: SessionEvent) {
        let Some(lane) = self.lanes.get_mut(lane_idx) else { return };
        let slot = if lane.active.as_ref().map(|s| s.id) == Some(id) {
            Slot::Active
        } else if lane.standby.as_ref().map(|s| s.id) == Some(id) {
            Slot::Standby
        } else if lane.draining.as_ref().map(|s| s.id) == Some(id) {
            Slot::Draining
        } else {
            return; // stale session (already replaced)
        };

        match ev {
            SessionEvent::Ready { expires_at_unix } => {
                let now = Instant::now();
                let expires_at = expires_at_unix.and_then(|exp| {
                    let now_unix = unix_ms() / 1000;
                    exp.checked_sub(now_unix).map(|left| now + Duration::from_secs(left))
                });
                let sess = match slot {
                    Slot::Active => lane.active.as_mut(),
                    Slot::Standby => lane.standby.as_mut(),
                    Slot::Draining => None,
                };
                if let Some(s) = sess {
                    s.ready_at = Some(now);
                    s.expires_at = expires_at;
                }
                if slot == Slot::Active {
                    lane.last_error = None;
                    lane.retry_at = None;
                    lane.retry_at = None;
                    // Single-switch: the target may have changed while connecting.
                    if self.cfg.strategy == BilingualStrategy::SingleSwitch {
                        if let Some(s) = lane.active.as_ref() {
                            let _ = s.tx.try_send(SessionCmd::SetTarget(lane.target));
                        }
                    }
                }
                self.update_status();
            }
            SessionEvent::Ended(reason) => {
                self.on_ended(lane_idx, slot, reason);
                self.update_status();
            }
            SessionEvent::ServerError { code, message } => {
                lane.last_error = Some(format!("{code}: {message}"));
            }
            ev @ (SessionEvent::SourceDelta { .. } | SessionEvent::OutputDelta { .. } | SessionEvent::OutputDone { .. }) => {
                if slot == Slot::Active && lane.draining.is_some() {
                    lane.held.push(ev);
                } else if slot != Slot::Standby {
                    self.route(lane_idx, slot, ev);
                }
            }
            SessionEvent::Updated => {}
        }
    }

    fn route(&mut self, lane_idx: usize, slot: Slot, ev: SessionEvent) {
        let (target, transcription) = {
            let l = &self.lanes[lane_idx];
            (l.target, l.transcription)
        };
        match ev {
            SessionEvent::SourceDelta { text, rx, .. } => {
                if !transcription {
                    return;
                }
                if let Some(new) = self.hub.source_delta(&text, rx) {
                    self.on_spoken_changed(new);
                }
            }
            SessionEvent::OutputDelta { text, elapsed_ms, rx } => {
                let now = Instant::now();
                self.metrics.incr(names::DELTAS, 1);
                let displayed = target != self.hub.spoken();
                if displayed {
                    if let Some(onset) = self.pending_onset.take() {
                        self.metrics.record(names::FIRST_DELTA, Basis::Estimated, now.duration_since(onset).as_secs_f64() * 1000.0);
                    }
                    if let Some(prev) = self.last_delta {
                        let gap = now.duration_since(prev);
                        if gap < DELTA_GAP {
                            self.metrics.record(names::DELTA_INTERVAL, Basis::Measured, gap.as_secs_f64() * 1000.0);
                        }
                    }
                    self.last_delta = Some(now);
                    self.last_delta_unix_ms = unix_ms();
                    let cap = {
                        let lane = &self.lanes[lane_idx];
                        let sess = match slot {
                            Slot::Active => lane.active.as_ref(),
                            Slot::Draining => lane.draining.as_ref(),
                            Slot::Standby => None,
                        };
                        sess.zip(elapsed_ms).and_then(|(s, e)| s.capture_time_of(e))
                    };
                    if let Some(cap) = cap.filter(|c| now > *c) {
                        self.metrics.record(names::PROVIDER_ALIGNED, Basis::Estimated, now.duration_since(cap).as_secs_f64() * 1000.0);
                    }
                }
                self.hub.translation_delta(target, &text, rx);
            }
            SessionEvent::OutputDone { text } => self.hub.translation_done(target, &text),
            _ => {}
        }
    }

    fn on_ended(&mut self, lane_idx: usize, slot: Slot, reason: crate::openai::EndReason) {
        use crate::openai::EndReason;
        let lane = &mut self.lanes[lane_idx];
        match slot {
            Slot::Draining => {
                lane.draining = None;
                self.flush_held(lane_idx);
            }
            Slot::Standby => {
                lane.standby = None;
                lane.rotate_retry_at = Some(Instant::now() + Duration::from_secs(10));
                if let EndReason::Fatal(m) = reason {
                    lane.fatal = Some(m);
                }
            }
            Slot::Active => {
                lane.active = None;
                if self.stopping {
                    return;
                }
                match reason {
                    EndReason::Fatal(m) => {
                        lane.fatal = Some(m.clone());
                        lane.last_error = Some(m);
                    }
                    EndReason::Stopped => {}
                    EndReason::Closed | EndReason::Failed(_) => {
                        if let EndReason::Failed(m) = &reason {
                            lane.last_error = Some(m.clone());
                        }
                        // A ready standby takes over instantly.
                        if let Some(sb) = lane.standby.take() {
                            if sb.ready_at.is_some() {
                                lane.active = Some(sb);
                                self.metrics.incr(names::ROTATIONS, 1);
                                return;
                            }
                        }
                        self.hub.end_all_segments();
                        self.schedule_retry(lane_idx);
                    }
                }
            }
        }
    }

    fn schedule_retry(&mut self, lane_idx: usize) {
        let lane = &mut self.lanes[lane_idx];
        lane.reconnects += 1;
        self.metrics.incr(names::RECONNECTS, 1);
        let jitter = 0.8 + rand::random::<f64>() * 0.4;
        let delay = (lane.backoff_ms as f64 * jitter) as u64;
        lane.retry_at = Some(Instant::now() + Duration::from_millis(delay));
        lane.backoff_ms = (lane.backoff_ms * 2).min(BACKOFF_MAX_MS);
    }

    fn flush_held(&mut self, lane_idx: usize) {
        let held = std::mem::take(&mut self.lanes[lane_idx].held);
        for ev in held {
            self.route(lane_idx, Slot::Active, ev);
        }
    }

    fn maybe_cutover(&mut self) {
        let now = Instant::now();
        let quiet = self.quiet_chunks >= 2;
        for idx in 0..self.lanes.len() {
            let lane = &mut self.lanes[idx];
            let standby_ready = lane.standby.as_ref().map(|s| s.ready_at.is_some()).unwrap_or(false);
            if !standby_ready || lane.draining.is_some() {
                continue;
            }
            let deadline = lane
                .active
                .as_ref()
                .and_then(|s| s.expires_at)
                .map(|e| now + ROTATE_DEADLINE >= e)
                .unwrap_or(true);
            if quiet || deadline {
                let old = lane.active.take();
                lane.active = lane.standby.take();
                if let Some(old) = old {
                    let _ = old.tx.try_send(SessionCmd::Close);
                    lane.draining = Some(old);
                    lane.cutover_at = Some(now);
                }
                self.metrics.incr(names::ROTATIONS, 1);
            }
        }
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        for idx in 0..self.lanes.len() {
            // (Re)connect.
            let need_connect = {
                let lane = &self.lanes[idx];
                lane.fatal.is_none() && lane.active.is_none() && lane.retry_at.map(|t| now >= t).unwrap_or(false)
            };
            if need_connect {
                let s = self.spawn_session(idx);
                let lane = &mut self.lanes[idx];
                lane.active = Some(s);
                lane.retry_at = None;
            }
            // Rotation before expiry.
            let need_standby = {
                let lane = &self.lanes[idx];
                lane.fatal.is_none()
                    && lane.standby.is_none()
                    && lane.draining.is_none()
                    && lane.rotate_retry_at.map(|t| now >= t).unwrap_or(true)
                    && lane
                        .active
                        .as_ref()
                        .and_then(|s| s.ready_at.and(s.expires_at))
                        .map(|e| now + ROTATE_BEFORE >= e)
                        .unwrap_or(false)
            };
            if need_standby {
                let s = self.spawn_session(idx);
                let lane = &mut self.lanes[idx];
                lane.standby = Some(s);
                lane.rotate_retry_at = None;
            }
            // Draining session that never finished.
            let drain_expired = {
                let lane = &self.lanes[idx];
                lane.draining.is_some() && lane.cutover_at.map(|t| now.duration_since(t) >= DRAIN_TIMEOUT).unwrap_or(true)
            };
            if drain_expired {
                self.lanes[idx].draining = None;
                self.flush_held(idx);
            }
            // Healthy for a while: reset backoff.
            let lane = &mut self.lanes[idx];
            if let Some(r) = lane.active.as_ref().and_then(|s| s.ready_at) {
                if now.duration_since(r) >= HEALTHY_RESET {
                    lane.backoff_ms = BACKOFF_START_MS;
                }
            }
        }
        // Without audio, cut-over is still needed before the deadline.
        if self.lanes.iter().any(|l| l.standby.as_ref().map(|s| s.ready_at.is_some()).unwrap_or(false)) {
            let deadline_hit = self.lanes.iter().any(|l| {
                l.active.as_ref().and_then(|s| s.expires_at).map(|e| now + ROTATE_DEADLINE >= e).unwrap_or(true)
            });
            if deadline_hit {
                self.maybe_cutover();
            }
        }
        self.update_status();
    }

    fn update_status(&mut self) {
        let now = Instant::now();
        let lanes: Vec<LaneStatus> = self
            .lanes
            .iter()
            .map(|l| {
                let state = if l.fatal.is_some() {
                    "failed"
                } else {
                    match l.active.as_ref() {
                        Some(s) if s.ready_at.is_some() => "ready",
                        Some(_) if l.reconnects == 0 => "connecting",
                        Some(_) | None => "reconnecting",
                    }
                };
                LaneStatus {
                    target: l.target,
                    transcription: l.transcription,
                    state,
                    reconnects: l.reconnects,
                    expires_in_s: l
                        .active
                        .as_ref()
                        .and_then(|s| s.expires_at)
                        .map(|e| if e > now { e.duration_since(now).as_secs() as i64 } else { 0 }),
                    last_error: l.last_error.clone(),
                }
            })
            .collect();
        let fatal = self.lanes.iter().find_map(|l| l.fatal.clone());
        let ready = lanes.iter().filter(|l| l.state == "ready").count();
        let state = if fatal.is_some() {
            "error"
        } else if ready == lanes.len() {
            "live"
        } else if lanes.iter().all(|l| l.state == "connecting") {
            "connecting"
        } else if ready > 0 {
            "degraded"
        } else {
            "reconnecting"
        };
        let message = match (&fatal, state) {
            (Some(m), _) => Some(friendly_fatal(m)),
            (None, "reconnecting") | (None, "degraded") => Some(format!(
                "Connection to OpenAI interrupted — reconnecting automatically ({} attempt(s)). Subtitles resume when it is back; switch to Soniox if it does not recover.",
                self.lanes.iter().map(|l| l.reconnects).sum::<u64>()
            )),
            _ => None,
        };
        let mut st = self.status.lock().unwrap();
        st.running = true;
        st.state = state;
        st.fatal = fatal.is_some();
        st.lanes = lanes;
        st.message = message;
        st.last_delta_unix_ms = self.last_delta_unix_ms;
    }
}

fn friendly_fatal(raw: &str) -> String {
    let r = raw.to_lowercase();
    if r.contains("auth") || r.contains("invalid_api_key") || r.contains("401") || r.contains("403") {
        "OpenAI rejected the API key. Check the key in Settings, then press Start again.".into()
    } else if r.contains("quota") {
        "OpenAI account quota exhausted. Add credit or switch to Soniox.".into()
    } else if r.contains("model_not_found") {
        "This OpenAI account cannot use gpt-realtime-translate.".into()
    } else {
        format!("OpenAI session stopped: {raw}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Active,
    Standby,
    Draining,
}
