//! One OpenAI realtime *translation* session over WebSocket.
//!
//! Protocol (verified against the official OpenAPI spec, `openai/openai-openapi`,
//! `RealtimeTranslation*` schemas):
//!
//! * URL `wss://api.openai.com/v1/realtime/translations?model=gpt-realtime-translate`,
//!   `Authorization: Bearer <key>`.
//! * Client events: `session.update` (only `audio.output.language`,
//!   `audio.input.transcription`, `audio.input.noise_reduction` are updatable),
//!   `session.input_audio_buffer.append` (base64 24 kHz PCM16 mono; the server
//!   consumes 200 ms frames and expects audio — including silence — to keep
//!   flowing), and `session.close` (server flushes pending audio, then closes).
//! * Server events: `session.created` (carries `expires_at`), `session.updated`,
//!   `session.closed`, `session.input_transcript.delta`,
//!   `session.output_transcript.delta`, `session.output_audio.delta`, `error`.
//!   Transcript deltas are append-only and carry `elapsed_ms` alignment
//!   metadata. There are NO `*.done` events and NO language-detection event in
//!   the spec; legacy `*.done` events are still handled defensively if received.
//! * There is no text-only output option; translated audio deltas are ignored
//!   here (never decoded, never played).

use crate::lang::Lang;
use crate::metrics::{names, Basis, Metrics};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message, Utf8Bytes};

pub const OPENAI_TRANSLATE_URL: &str =
    "wss://api.openai.com/v1/realtime/translations?model=gpt-realtime-translate";
pub const TRANSCRIPTION_MODEL: &str = "gpt-realtime-whisper";

const PING_EVERY: Duration = Duration::from_secs(10);
const DEAD_AFTER: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct SessionSpec {
    pub url: String,
    pub api_key: String,
    pub target: Lang,
    pub transcription: bool,
    /// "near_field" | "far_field" | None.
    pub noise_reduction: Option<String>,
}

#[derive(Debug)]
pub enum SessionCmd {
    /// Pre-built `session.input_audio_buffer.append` JSON (shared across
    /// sessions without copying) plus the chunk's audio duration and the
    /// capture time of its oldest sample.
    Audio { msg: Utf8Bytes, ms: u32, oldest: Instant },
    SetTarget(Lang),
    /// Graceful `session.close`: flush pending output, then end.
    Close,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EndReason {
    /// Server closed the session normally (or after our `session.close`).
    Closed,
    /// Credentials rejected: never retry automatically.
    Fatal(String),
    /// Network / protocol failure: retry with backoff.
    Failed(String),
    /// Our command channel was dropped (engine stopped).
    Stopped,
}

#[derive(Debug)]
pub enum SessionEvent {
    Ready { expires_at_unix: Option<u64> },
    Updated,
    SourceDelta { text: String, elapsed_ms: Option<u64>, rx: Instant },
    OutputDelta { text: String, elapsed_ms: Option<u64>, rx: Instant },
    OutputDone { text: String },
    ServerError { code: String, message: String },
    Ended(EndReason),
}

/// (lane, session id, event)
pub type Tagged = (usize, u64, SessionEvent);

pub fn audio_append_message(pcm: &[u8]) -> Utf8Bytes {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(pcm);
    let mut s = String::with_capacity(b64.len() + 64);
    s.push_str(r#"{"type":"session.input_audio_buffer.append","audio":""#);
    s.push_str(&b64);
    s.push_str(r#""}"#);
    Utf8Bytes::from(s)
}

pub fn session_update_message(target: Lang, transcription: bool, noise_reduction: Option<&str>) -> String {
    let transcription = if transcription {
        serde_json::json!({ "model": TRANSCRIPTION_MODEL })
    } else {
        Value::Null
    };
    let nr = match noise_reduction {
        Some(t) if !t.is_empty() => serde_json::json!({ "type": t }),
        _ => Value::Null,
    };
    serde_json::json!({
        "type": "session.update",
        "session": {
            "audio": {
                "input": { "transcription": transcription, "noise_reduction": nr },
                "output": { "language": target.code() }
            }
        }
    })
    .to_string()
}

fn target_update_message(target: Lang) -> String {
    serde_json::json!({
        "type": "session.update",
        "session": { "audio": { "output": { "language": target.code() } } }
    })
    .to_string()
}

/// Errors that must stop automatic reconnection (avoid hammering the API or
/// running up charges with a configuration that can never work).
pub fn is_fatal_code(code: &str) -> bool {
    matches!(
        code,
        "invalid_api_key" | "insufficient_quota" | "model_not_found" | "account_deactivated" | "unauthorized"
    )
}

/// Run one session until it ends. Events are tagged and forwarded to `ev_tx`;
/// the final event is always `Ended`.
pub async fn run_session(
    lane: usize,
    id: u64,
    spec: SessionSpec,
    cmd_rx: mpsc::Receiver<SessionCmd>,
    ev_tx: mpsc::Sender<Tagged>,
    metrics: Arc<Metrics>,
) {
    let reason = session_inner(lane, id, &spec, cmd_rx, &ev_tx, &metrics).await;
    let _ = ev_tx.send((lane, id, SessionEvent::Ended(reason))).await;
}

async fn session_inner(
    lane: usize,
    id: u64,
    spec: &SessionSpec,
    mut cmd_rx: mpsc::Receiver<SessionCmd>,
    ev_tx: &mpsc::Sender<Tagged>,
    metrics: &Metrics,
) -> EndReason {
    let mut request = match spec.url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => return EndReason::Fatal(format!("invalid URL: {e}")),
    };
    match format!("Bearer {}", spec.api_key).parse() {
        Ok(v) => {
            request.headers_mut().insert("Authorization", v);
        }
        Err(_) => return EndReason::Fatal("API key contains invalid characters".into()),
    }

    let connect = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await;
    let ws = match connect {
        Err(_) => return EndReason::Failed("connect timeout".into()),
        Ok(Err(tungstenite::Error::Http(resp))) => {
            let status = resp.status().as_u16();
            return if status == 401 || status == 403 {
                EndReason::Fatal(format!("authentication rejected (HTTP {status})"))
            } else {
                EndReason::Failed(format!("HTTP {status} during WebSocket upgrade"))
            };
        }
        Ok(Err(e)) => return EndReason::Failed(format!("connect: {e}")),
        Ok(Ok((ws, _))) => ws,
    };
    let (mut sink, mut stream) = ws.split();

    let update = session_update_message(spec.target, spec.transcription, spec.noise_reduction.as_deref());
    if let Err(e) = sink.send(Message::Text(update.into())).await {
        return EndReason::Failed(format!("send session.update: {e}"));
    }

    let send = |ev: SessionEvent| ev_tx.send((lane, id, ev));
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // first tick is immediate
    let mut last_rx = Instant::now();
    let mut ping_sent: Option<Instant> = None;
    let mut closing: Option<Instant> = None;
    let mut ready_sent = false;

    loop {
        let close_deadline = async {
            match closing {
                Some(t) => tokio::time::sleep_until((t + CLOSE_GRACE).into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            cmd = cmd_rx.recv(), if closing.is_none() => {
                match cmd {
                    None => {
                        let _ = sink.send(Message::Close(None)).await;
                        return EndReason::Stopped;
                    }
                    Some(SessionCmd::Audio { msg, .. }) => {
                        let t = Instant::now();
                        if let Err(e) = sink.send(Message::Text(msg)).await {
                            return EndReason::Failed(format!("send audio: {e}"));
                        }
                        metrics.record(names::AUDIO_SEND, Basis::Measured, t.elapsed().as_secs_f64() * 1000.0);
                    }
                    Some(SessionCmd::SetTarget(l)) => {
                        if let Err(e) = sink.send(Message::Text(target_update_message(l).into())).await {
                            return EndReason::Failed(format!("send session.update: {e}"));
                        }
                    }
                    Some(SessionCmd::Close) => {
                        let _ = sink.send(Message::Text(r#"{"type":"session.close"}"#.into())).await;
                        closing = Some(Instant::now());
                    }
                }
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > DEAD_AFTER {
                    return EndReason::Failed("heartbeat timeout (no data or pong for 30 s)".into());
                }
                ping_sent = Some(Instant::now());
                if let Err(e) = sink.send(Message::Ping(Vec::new().into())).await {
                    return EndReason::Failed(format!("ping: {e}"));
                }
            }
            _ = close_deadline => {
                let _ = sink.send(Message::Close(None)).await;
                return EndReason::Closed;
            }
            msg = stream.next() => {
                last_rx = Instant::now();
                match msg {
                    None => {
                        return if closing.is_some() { EndReason::Closed } else { EndReason::Failed("connection dropped".into()) };
                    }
                    Some(Err(e)) => {
                        return if closing.is_some() { EndReason::Closed } else { EndReason::Failed(format!("ws: {e}")) };
                    }
                    Some(Ok(Message::Pong(_))) => {
                        if let Some(t) = ping_sent.take() {
                            metrics.record(names::NETWORK_RTT, Basis::Measured, t.elapsed().as_secs_f64() * 1000.0);
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        if closing.is_some() {
                            return EndReason::Closed;
                        }
                        let reason = frame.map(|f| format!("{} {}", u16::from(f.code), f.reason)).unwrap_or_default();
                        // 1008 (policy) / 4001-style codes usually mean auth problems.
                        if reason.contains("invalid_api_key") || reason.starts_with("4001") {
                            return EndReason::Fatal(format!("server closed: {reason}"));
                        }
                        return EndReason::Failed(format!("server closed: {reason}"));
                    }
                    Some(Ok(Message::Text(text))) => {
                        let rx = Instant::now();
                        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
                        let delta = || v.get("delta").and_then(Value::as_str).unwrap_or("").to_string();
                        let elapsed = || v.get("elapsed_ms").and_then(Value::as_u64);
                        let ev = match ty {
                            "session.created" => {
                                ready_sent = true;
                                let exp = v.pointer("/session/expires_at").and_then(Value::as_u64);
                                Some(SessionEvent::Ready { expires_at_unix: exp })
                            }
                            "session.updated" => {
                                if ready_sent { Some(SessionEvent::Updated) } else {
                                    ready_sent = true;
                                    let exp = v.pointer("/session/expires_at").and_then(Value::as_u64);
                                    Some(SessionEvent::Ready { expires_at_unix: exp })
                                }
                            }
                            "session.input_transcript.delta" => Some(SessionEvent::SourceDelta { text: delta(), elapsed_ms: elapsed(), rx }),
                            "session.output_transcript.delta" => Some(SessionEvent::OutputDelta { text: delta(), elapsed_ms: elapsed(), rx }),
                            "session.output_transcript.done" => {
                                v.get("transcript").and_then(Value::as_str).map(|t| SessionEvent::OutputDone { text: t.to_string() })
                            }
                            "session.output_audio.delta" | "session.input_transcript.done" => None,
                            "session.closed" => return EndReason::Closed,
                            "error" => {
                                let code = v.pointer("/error/code").and_then(Value::as_str).unwrap_or("unknown").to_string();
                                let message = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("").to_string();
                                if is_fatal_code(&code) {
                                    let _ = send(SessionEvent::ServerError { code: code.clone(), message: message.clone() }).await;
                                    let _ = sink.send(Message::Close(None)).await;
                                    return EndReason::Fatal(format!("{code}: {message}"));
                                }
                                Some(SessionEvent::ServerError { code, message })
                            }
                            _ => None,
                        };
                        if let Some(ev) = ev {
                            if send(ev).await.is_err() {
                                let _ = sink.send(Message::Close(None)).await;
                                return EndReason::Stopped;
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_message_shape() {
        let m = audio_append_message(&[0, 0, 1, 0]);
        let v: Value = serde_json::from_str(m.as_str()).unwrap();
        assert_eq!(v["type"], "session.input_audio_buffer.append");
        assert_eq!(v["audio"], "AAABAA==");
    }

    #[test]
    fn session_update_shape() {
        let v: Value = serde_json::from_str(&session_update_message(Lang::Vi, true, Some("near_field"))).unwrap();
        assert_eq!(v["type"], "session.update");
        assert_eq!(v["session"]["audio"]["output"]["language"], "vi");
        assert_eq!(v["session"]["audio"]["input"]["transcription"]["model"], TRANSCRIPTION_MODEL);
        assert_eq!(v["session"]["audio"]["input"]["noise_reduction"]["type"], "near_field");
        // No undocumented fields (prompt, modalities, voice, turn_detection…).
        assert!(v["session"].get("instructions").is_none());
        assert!(v["session"].get("output_modalities").is_none());

        let v: Value = serde_json::from_str(&session_update_message(Lang::En, false, None)).unwrap();
        assert!(v["session"]["audio"]["input"]["transcription"].is_null());
        assert!(v["session"]["audio"]["input"]["noise_reduction"].is_null());
    }
}
