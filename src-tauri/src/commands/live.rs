//! Live-event mode: bilingual broadcast subtitles for vMix / OBS.
//!
//! The OpenAI path runs entirely in Rust (capture → engine → caption hub →
//! overlay server), so subtitles keep flowing even if the operator window is
//! minimised or its webview is throttled. The Soniox fallback runs in the
//! webview (existing client) and feeds the same caption hub through
//! `live_soniox_ingest`, so the overlay URLs never change when switching.

use crate::audio::microphone::AudioStatusView;
use crate::settings::SettingsState;
use live_core::config::{LiveSettings, Provider};
use live_core::engine::{EngineConfig, EngineStatus, LiveEngine};
use live_core::metrics::MetricsSnapshot;
use live_core::overlay::{parse_control, ControlHandler, ControlRequest, OverlayServer, ServerOptions};
use live_core::router::{CompletedSegment, RouterStatus, Stream};
use live_core::{CaptionHub, Lang, Metrics};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::ipc::Channel;
use tauri::State;

pub struct LiveInner {
    pub hub: Arc<CaptionHub>,
    pub metrics: Arc<Metrics>,
    pub capture: Arc<crate::audio::capture::AudioCapture>,
    engine: Mutex<Option<Arc<LiveEngine>>>,
    provider: Mutex<Option<Provider>>,
    /// Translation mode the running sessions were started with.
    running_mode: Mutex<Option<live_core::config::TranslationMode>>,
    started_at: Mutex<Option<Instant>>,
    server: tokio::sync::Mutex<Option<OverlayServer>>,
    server_error: Mutex<Option<String>>,
    segments_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct LiveState(pub Arc<LiveInner>);

impl LiveState {
    pub fn new(settings: &LiveSettings, metrics: Arc<Metrics>, capture: Arc<crate::audio::capture::AudioCapture>) -> Self {
        let hub = CaptionHub::new(settings, metrics.clone());
        Self(Arc::new(LiveInner {
            hub,
            metrics,
            capture,
            engine: Mutex::new(None),
            provider: Mutex::new(None),
            running_mode: Mutex::new(None),
            started_at: Mutex::new(None),
            server: tokio::sync::Mutex::new(None),
            server_error: Mutex::new(None),
            segments_task: Mutex::new(None),
        }))
    }
}

impl LiveInner {
    fn engine(&self) -> Option<Arc<LiveEngine>> {
        self.engine.lock().unwrap().clone()
    }

    /// Push saved settings to the caption hub. While a session runs, its
    /// translation mode stays fixed until the next Start (sessions were opened
    /// for that mode); layout and caption behaviour apply immediately.
    pub fn apply_settings(&self, live: &LiveSettings) {
        let mut applied = live.clone();
        if let Some(mode) = *self.running_mode.lock().unwrap() {
            applied.mode = mode;
        }
        self.hub.apply_settings(&applied);
    }

    pub fn apply_control(&self, req: ControlRequest) {
        match req {
            ControlRequest::Caption(c) => self.hub.control(c),
            ControlRequest::Language(l) => {
                let changed = self.hub.set_override(l);
                if let (Some(lang), Some(e)) = (changed, self.engine()) {
                    e.spoken_changed(lang);
                }
            }
        }
    }

    fn control_handler(self: &Arc<Self>) -> ControlHandler {
        let me = Arc::downgrade(self);
        Arc::new(move |req| {
            if let Some(me) = me.upgrade() {
                me.apply_control(req);
            }
        })
    }

    /// Start (or restart with new options) the overlay server.
    pub async fn ensure_server(self: &Arc<Self>, live: &LiveSettings) -> Result<(), String> {
        let opts = ServerOptions {
            port: live.server.port,
            allow_lan: live.server.allow_lan,
            token: live.server.access_token.clone(),
        };
        let mut guard = self.server.lock().await;
        if let Some(s) = guard.as_ref() {
            let o = s.options();
            if o.port == opts.port && o.allow_lan == opts.allow_lan && o.token == opts.token {
                return Ok(());
            }
        }
        if let Some(old) = guard.take() {
            old.stop().await;
        }
        match OverlayServer::start(opts, self.hub.clone(), self.control_handler()).await {
            Ok(s) => {
                *guard = Some(s);
                *self.server_error.lock().unwrap() = None;
                Ok(())
            }
            Err(e) => {
                *self.server_error.lock().unwrap() = Some(e.clone());
                Err(e)
            }
        }
    }

    pub async fn stop_server(&self) {
        if let Some(s) = self.server.lock().await.take() {
            s.stop().await;
        }
    }

    pub async fn stop_engine(&self) {
        let engine = self.engine.lock().unwrap().take();
        self.capture.detach_live();
        if let Some(e) = engine {
            e.stop().await;
        }
    }

    /// Full shutdown (app exit): close sessions gracefully, stop the server.
    pub async fn shutdown(&self) {
        self.stop_engine().await;
        *self.provider.lock().unwrap() = None;
        self.stop_server().await;
        self.capture.stop_all();
    }
}

fn settings_snapshot(settings: &State<'_, SettingsState>) -> Result<crate::settings::Settings, String> {
    Ok(settings.0.lock().map_err(|e| e.to_string())?.clone())
}

#[derive(Debug, Deserialize)]
pub struct LiveStartArgs {
    pub provider: Provider,
}

/// Start live subtitles. For `openai` this starts capture + engine in Rust.
/// For `soniox` it only marks the mode active; the webview runs the Soniox
/// client and feeds captions through `live_soniox_ingest`.
#[tauri::command]
pub async fn live_start(
    args: LiveStartArgs,
    live: State<'_, LiveState>,
    settings: State<'_, SettingsState>,
) -> Result<LiveStatus, String> {
    let s = settings_snapshot(&settings)?;
    let cfg = s.live.clone().sanitized();
    let inner = live.0.clone();
    *inner.running_mode.lock().unwrap() = None;
    inner.hub.apply_settings(&cfg);
    if let Err(e) = inner.ensure_server(&cfg).await {
        eprintln!("[live] overlay server: {e}");
    }
    // Never run two providers at once (duplicate captions / double billing).
    inner.stop_engine().await;
    match args.provider {
        Provider::Openai => {
            if s.openai_api_key.trim().is_empty() {
                return Err("OpenAI API key is missing. Add it in Settings → OpenAI.".into());
            }
            let mut ecfg = EngineConfig::new(s.openai_api_key.trim(), cfg.mode, cfg.strategy);
            ecfg.noise_reduction = Some(cfg.noise_reduction.clone()).filter(|n| !n.is_empty());
            ecfg.chunk_ms = cfg.openai_chunk_ms;
            let engine = Arc::new(LiveEngine::start(ecfg, inner.hub.clone(), inner.metrics.clone()));
            let sink_engine = Arc::downgrade(&engine);
            let device = Some(cfg.input_device.clone()).filter(|d| !d.is_empty());
            let attach = inner.capture.attach_live(
                "microphone",
                device,
                cfg.openai_chunk_ms,
                Arc::new(move |chunk| {
                    if let Some(e) = sink_engine.upgrade() {
                        e.push_audio(chunk);
                    }
                }),
            );
            if let Err(e) = attach {
                engine.stop().await;
                return Err(e);
            }
            *inner.engine.lock().unwrap() = Some(engine);
        }
        Provider::Soniox => {
            if s.soniox_api_key.trim().is_empty() {
                return Err("Soniox API key is missing. Add it in Settings → Soniox.".into());
            }
        }
    }
    *inner.provider.lock().unwrap() = Some(args.provider);
    *inner.running_mode.lock().unwrap() = Some(cfg.mode);
    *inner.started_at.lock().unwrap() = Some(Instant::now());
    Ok(build_status(&inner, &cfg, live_audio(&inner)).await)
}

#[tauri::command]
pub async fn live_stop(live: State<'_, LiveState>) -> Result<(), String> {
    let inner = live.0.clone();
    inner.stop_engine().await;
    *inner.provider.lock().unwrap() = None;
    *inner.running_mode.lock().unwrap() = None;
    *inner.started_at.lock().unwrap() = None;
    Ok(())
}

/// hide | show | toggle | freeze | unfreeze | toggle-freeze | clear | lang-en | lang-vi | lang-auto
#[tauri::command]
pub fn live_control(action: String, live: State<'_, LiveState>) -> Result<(), String> {
    let req = parse_control(&action).ok_or_else(|| format!("unknown action: {action}"))?;
    live.0.apply_control(req);
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct SonioxIngest {
    /// "source" | "translation"
    pub stream: String,
    pub lang: Option<String>,
    #[serde(default)]
    pub stable: String,
    #[serde(default)]
    pub tentative: String,
    #[serde(default)]
    pub end: bool,
}

/// Captions from the Soniox client running in the webview.
#[tauri::command]
pub fn live_soniox_ingest(items: Vec<SonioxIngest>, live: State<'_, LiveState>) -> Result<(), String> {
    for it in items {
        let stream = if it.stream == "source" { Stream::Source } else { Stream::Translation };
        let lang = it.lang.as_deref().and_then(Lang::from_code);
        live.0.hub.ingest_tokens(stream, lang, &it.stable, &it.tentative, it.end);
    }
    Ok(())
}

/// Stream finalised caption segments to the operator console.
#[tauri::command]
pub fn live_subscribe_segments(channel: Channel<CompletedSegment>, live: State<'_, LiveState>) {
    let inner = live.0.clone();
    let mut rx = inner.hub.subscribe_segments();
    let task = tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(seg) => {
                    if channel.send(seg).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
    let old = inner.segments_task.lock().unwrap().replace(task);
    if let Some(old) = old {
        old.abort();
    }
}

#[derive(Serialize)]
pub struct ServerInfo {
    pub running: bool,
    pub port: u16,
    pub allow_lan: bool,
    pub error: Option<String>,
    pub urls: Vec<(String, String)>,
    pub control_token: String,
}

#[derive(Serialize)]
pub struct LiveStatus {
    pub provider: Option<Provider>,
    pub uptime_s: u64,
    pub engine: Option<EngineStatus>,
    pub router: RouterStatus,
    pub audio: Option<AudioStatusView>,
    pub metrics: MetricsSnapshot,
    pub server: ServerInfo,
    pub memory_mb: Option<f64>,
}

fn live_audio(inner: &LiveInner) -> Option<AudioStatusView> {
    Some(inner.capture.stats.view())
}

async fn build_status(inner: &Arc<LiveInner>, cfg: &LiveSettings, audio: Option<AudioStatusView>) -> LiveStatus {
    let server = {
        let guard = inner.server.lock().await;
        let (running, port, allow_lan) = match guard.as_ref() {
            Some(s) => (true, s.addr().port(), s.options().allow_lan),
            None => (false, cfg.server.port, cfg.server.allow_lan),
        };
        let host = "127.0.0.1";
        let tok = if allow_lan { format!("&token={}", cfg.server.access_token) } else { String::new() };
        let urls = ["left", "right", "both", "canvas"]
            .iter()
            .map(|s| (s.to_string(), format!("http://{host}:{port}/overlay?screen={s}{tok}")))
            .collect();
        ServerInfo {
            running,
            port,
            allow_lan,
            error: inner.server_error.lock().unwrap().clone(),
            urls,
            control_token: cfg.server.access_token.clone(),
        }
    };
    LiveStatus {
        provider: *inner.provider.lock().unwrap(),
        uptime_s: inner.started_at.lock().unwrap().map(|t| t.elapsed().as_secs()).unwrap_or(0),
        engine: inner.engine().map(|e| e.status()),
        router: inner.hub.status(),
        audio,
        metrics: inner.metrics.snapshot(),
        server,
        memory_mb: crate::sysinfo::process_memory_mb(),
    }
}

#[tauri::command]
pub async fn live_status(live: State<'_, LiveState>, settings: State<'_, SettingsState>) -> Result<LiveStatus, String> {
    let cfg = settings_snapshot(&settings)?.live;
    let inner = live.0.clone();
    let audio = live_audio(&inner);
    Ok(build_status(&inner, &cfg, audio).await)
}

#[tauri::command]
pub async fn live_server_restart(live: State<'_, LiveState>, settings: State<'_, SettingsState>) -> Result<(), String> {
    let cfg = settings_snapshot(&settings)?.live.sanitized();
    let inner = live.0.clone();
    inner.stop_server().await;
    inner.ensure_server(&cfg).await
}

#[tauri::command]
pub async fn live_server_stop(live: State<'_, LiveState>) -> Result<(), String> {
    live.0.stop_server().await;
    Ok(())
}

#[tauri::command]
pub fn live_reset_metrics(live: State<'_, LiveState>) {
    live.0.metrics.reset();
}
