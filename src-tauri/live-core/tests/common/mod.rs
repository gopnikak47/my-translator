//! Mock of the OpenAI realtime translation WebSocket endpoint.
//!
//! Implements the documented protocol surface: Bearer auth on upgrade,
//! `session.created` (with `expires_at`), `session.update` → `session.updated`,
//! `session.input_audio_buffer.append`, `session.close`, and scripted
//! `session.input_transcript.delta` / `session.output_transcript.delta` events.

#![allow(dead_code)]

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;

pub const GOOD_KEY: &str = "sk-test-good";

/// Events a session sends after receiving its N-th audio append (1-based).
#[derive(Clone, Default)]
pub struct Script {
    /// target language → (only on connection #, append index, events (type, delta))
    pub by_target: HashMap<String, Vec<(Option<usize>, usize, Vec<(&'static str, String)>)>>,
    /// Sent (as output deltas) when the client sends `session.close`.
    pub on_close: HashMap<String, Vec<String>>,
}

impl Script {
    pub fn at(self, target: &str, append: usize, events: Vec<(&'static str, &str)>) -> Self {
        self.push(None, target, append, events)
    }
    /// Like `at`, but only on connection `conn` (0-based).
    pub fn at_conn(self, conn: usize, target: &str, append: usize, events: Vec<(&'static str, &str)>) -> Self {
        self.push(Some(conn), target, append, events)
    }
    fn push(mut self, conn: Option<usize>, target: &str, append: usize, events: Vec<(&'static str, &str)>) -> Self {
        self.by_target
            .entry(target.to_string())
            .or_default()
            .push((conn, append, events.into_iter().map(|(t, d)| (t, d.to_string())).collect()));
        self
    }
    pub fn on_close(mut self, target: &str, deltas: Vec<&str>) -> Self {
        self.on_close.insert(target.to_string(), deltas.into_iter().map(String::from).collect());
        self
    }
}

#[derive(Default)]
pub struct Log {
    pub session_updates: Vec<Value>,
    pub appends: Vec<(usize, usize)>, // (connection index, decoded bytes)
    pub closes: Vec<usize>,
}

pub struct MockOpenAi {
    pub addr: SocketAddr,
    pub log: Arc<Mutex<Log>>,
    pub connections: Arc<AtomicUsize>,
    pub concurrent: Arc<AtomicUsize>,
    pub max_concurrent: Arc<AtomicUsize>,
}

#[derive(Clone, Default)]
pub struct Options {
    pub script: Script,
    /// Drop connection #idx (0-based) after this many appends.
    pub drop_conn_after: Option<(usize, usize)>,
    /// expires_at offset (seconds from now) per connection index; default 3600.
    pub expires_in: HashMap<usize, u64>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

impl MockOpenAi {
    pub async fn start(opts: Options) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Log::default()));
        let connections = Arc::new(AtomicUsize::new(0));
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let (l, c, cc, mc) = (log.clone(), connections.clone(), concurrent.clone(), max_concurrent.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let conn_idx = c.fetch_add(1, Ordering::SeqCst);
                let (log, opts, cc, mc) = (l.clone(), opts.clone(), cc.clone(), mc.clone());
                tokio::spawn(async move {
                    let auth_ok = Arc::new(Mutex::new(false));
                    let a2 = auth_ok.clone();
                    let cb = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                        let ok = req.headers().get("authorization").and_then(|v| v.to_str().ok())
                            == Some(&format!("Bearer {GOOD_KEY}"));
                        assert!(req.uri().path().ends_with("/v1/realtime/translations"));
                        if ok {
                            *a2.lock().unwrap() = true;
                            Ok(resp)
                        } else {
                            let mut r = ErrorResponse::new(Some("invalid_api_key".into()));
                            *r.status_mut() = http::StatusCode::UNAUTHORIZED;
                            Err(r)
                        }
                    };
                    let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await else { return };
                    let n = cc.fetch_add(1, Ordering::SeqCst) + 1;
                    mc.fetch_max(n, Ordering::SeqCst);
                    serve(conn_idx, ws, opts, log).await;
                    cc.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Self { addr, log, connections, concurrent, max_concurrent }
    }

    pub fn url(&self) -> String {
        format!("ws://{}/v1/realtime/translations?model=gpt-realtime-translate", self.addr)
    }
}

async fn serve(
    idx: usize,
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    opts: Options,
    log: Arc<Mutex<Log>>,
) {
    let (mut tx, mut rx) = ws.split();
    let exp = now_unix() + opts.expires_in.get(&idx).copied().unwrap_or(3600);
    let created = json!({"type":"session.created","event_id":"e0","session":{"id":format!("sess_{idx}"),"type":"translation","model":"gpt-realtime-translate","expires_at":exp,"audio":{"output":{"language":"es"}}}});
    if tx.send(Message::Text(created.to_string().into())).await.is_err() {
        return;
    }
    let mut target = String::from("es");
    let mut appends = 0usize;
    while let Some(Ok(msg)) = rx.next().await {
        let Message::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        match v["type"].as_str().unwrap_or("") {
            "session.update" => {
                if let Some(l) = v.pointer("/session/audio/output/language").and_then(Value::as_str) {
                    target = l.to_string();
                }
                log.lock().unwrap().session_updates.push(v.clone());
                let upd = json!({"type":"session.updated","event_id":"e1","session":{"id":format!("sess_{idx}"),"type":"translation","model":"gpt-realtime-translate","expires_at":exp,"audio":{"output":{"language":target}}}});
                let _ = tx.send(Message::Text(upd.to_string().into())).await;
            }
            "session.input_audio_buffer.append" => {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD.decode(v["audio"].as_str().unwrap()).unwrap();
                appends += 1;
                log.lock().unwrap().appends.push((idx, bytes.len()));
                if let Some(evs) = opts.script.by_target.get(&target) {
                    for (conn, at, list) in evs {
                        if *at == appends && conn.map(|c| c == idx).unwrap_or(true) {
                            for (ty, d) in list {
                                let ev = json!({"type": ty, "event_id":"x", "delta": d, "elapsed_ms": (appends as u64 - 1) * 200});
                                let _ = tx.send(Message::Text(ev.to_string().into())).await;
                            }
                        }
                    }
                }
                if let Some((ci, after)) = opts.drop_conn_after {
                    if ci == idx && appends >= after {
                        return; // abrupt drop (no close frame)
                    }
                }
            }
            "session.close" => {
                log.lock().unwrap().closes.push(idx);
                if let Some(ds) = opts.script.on_close.get(&target) {
                    for d in ds {
                        let ev = json!({"type":"session.output_transcript.delta","event_id":"x","delta":d});
                        let _ = tx.send(Message::Text(ev.to_string().into())).await;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = tx.send(Message::Text(json!({"type":"session.closed","event_id":"c"}).to_string().into())).await;
                let _ = tx.send(Message::Close(None)).await;
                return;
            }
            _ => {}
        }
    }
}

/// 200 ms of 24 kHz PCM16: a tone (speech-like level) or silence.
pub fn chunk(loud: bool) -> live_core::audio::Chunk {
    let n = 4800;
    let mut pcm = Vec::with_capacity(n * 2);
    for i in 0..n {
        let s = if loud { ((i as f32 * 0.07).sin() * 8000.0) as i16 } else { 0 };
        pcm.extend_from_slice(&s.to_le_bytes());
    }
    live_core::audio::Chunk { pcm, oldest: std::time::Instant::now(), samples: n }
}

pub async fn wait_until<F: FnMut() -> bool>(timeout_ms: u64, mut f: F) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    f()
}
