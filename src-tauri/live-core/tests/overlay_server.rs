//! Overlay HTTP/SSE server tests: routes, security defaults, push updates,
//! multiple clients, reconnection and shutdown.

use live_core::config::{LiveSettings, TranslationMode};
use live_core::metrics::names;
use live_core::overlay::{hub_control_handler, OverlayServer, ServerOptions};
use live_core::router::ScreenId;
use live_core::{CaptionHub, Lang, Metrics};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "0123456789abcdef0123";

async fn start(allow_lan: bool) -> (OverlayServer, Arc<CaptionHub>) {
    let mut s = LiveSettings::default();
    s.mode = TranslationMode::EnToVi;
    let hub = CaptionHub::new(&s, Arc::new(Metrics::new()));
    let srv = OverlayServer::start(
        ServerOptions { port: 0, allow_lan, token: TOKEN.into() },
        hub.clone(),
        hub_control_handler(hub.clone()),
    )
    .await
    .unwrap();
    (srv, hub)
}

async fn request(port: u16, raw: String) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(raw.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn get(port: u16, path: &str, extra: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}\r\n")
}

struct Sse {
    s: TcpStream,
    buf: String,
    all: String,
}

impl Sse {
    async fn open(port: u16, screen: &str) -> Sse {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(get(port, &format!("/events?screen={screen}"), "Accept: text/event-stream\r\n").as_bytes())
            .await
            .unwrap();
        Sse { s, buf: String::new(), all: String::new() }
    }

    /// Read until the buffer contains `needle` (or timeout). Consumes up to it.
    async fn expect(&mut self, needle: &str, ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            if let Some(i) = self.buf.find(needle) {
                self.buf.drain(..i + needle.len());
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let mut tmp = [0u8; 4096];
            match tokio::time::timeout(left, self.s.read(&mut tmp)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return self.buf.contains(needle),
                Ok(Ok(n)) => {
                    let chunk = String::from_utf8_lossy(&tmp[..n]).into_owned();
                    self.all.push_str(&chunk);
                    self.buf.push_str(&chunk);
                }
            }
        }
    }

    async fn closed_within(&mut self, ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(ms);
        let mut tmp = [0u8; 4096];
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(100), self.s.read(&mut tmp)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                _ => {}
            }
        }
        false
    }
}

#[tokio::test]
async fn serves_overlay_page_with_security_headers() {
    let (srv, _hub) = start(false).await;
    let port = srv.addr().port();
    assert!(srv.addr().ip().is_loopback(), "binds localhost by default");
    for screen in ["left", "right", "both", "canvas"] {
        let r = request(port, get(port, &format!("/overlay?screen={screen}"), "")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{screen}: {r}");
        assert!(r.contains("Content-Security-Policy:"));
        assert!(r.contains("Cache-Control: no-store"));
        assert!(r.contains("overlay.js"));
        assert!(!r.to_lowercase().contains("access-control-allow-origin"));
    }
    let js = request(port, get(port, "/overlay.js", "")).await;
    assert!(js.starts_with("HTTP/1.1 200") && js.contains("EventSource"));
    let health = request(port, get(port, "/health", "")).await;
    assert!(health.contains(r#"{"ok":true}"#));
    let nf = request(port, get(port, "/secret", "")).await;
    assert!(nf.starts_with("HTTP/1.1 404"));
    srv.stop().await;
}

#[tokio::test]
async fn rejects_foreign_host_and_origin() {
    let (srv, _hub) = start(false).await;
    let port = srv.addr().port();
    // DNS rebinding: attacker domain resolving to 127.0.0.1.
    let r = request(port, format!("GET /overlay HTTP/1.1\r\nHost: evil.example:{port}\r\n\r\n")).await;
    assert!(r.starts_with("HTTP/1.1 403"), "{r}");
    // Cross-site page trying to read the event stream.
    let r = request(port, get(port, "/events?screen=left", "Origin: https://evil.example\r\n")).await;
    assert!(r.starts_with("HTTP/1.1 403"), "{r}");
    // Same-origin page is fine.
    let r = request(port, get(port, "/health", &format!("Origin: http://127.0.0.1:{port}\r\n"))).await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    // Oversized header block.
    let big = format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX: {}\r\n\r\n", "a".repeat(10_000));
    let r = request(port, big).await;
    assert!(r.starts_with("HTTP/1.1 431"), "{r}");
    srv.stop().await;
}

#[tokio::test]
async fn control_requires_token() {
    let (srv, hub) = start(false).await;
    let port = srv.addr().port();
    let post = |path: &str, extra: &str| {
        format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\n{extra}\r\n")
    };
    let r = request(port, post("/control/hide", "")).await;
    assert!(r.starts_with("HTTP/1.1 401"), "{r}");
    assert!(hub.current_frame(ScreenId::Left).visible);
    let r = request(port, post("/control/hide", "X-Access-Token: wrong-token-wrong\r\n")).await;
    assert!(r.starts_with("HTTP/1.1 401"));
    let r = request(port, post("/control/hide", &format!("X-Access-Token: {TOKEN}\r\n"))).await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    assert!(!hub.current_frame(ScreenId::Left).visible);
    let r = request(port, post(&format!("/control/lang-vi?token={TOKEN}"), "")).await;
    assert!(r.starts_with("HTTP/1.1 200"));
    assert_eq!(hub.spoken(), Lang::Vi);
    let r = request(port, post(&format!("/control/format-disk?token={TOKEN}"), "")).await;
    assert!(r.starts_with("HTTP/1.1 404"));
    srv.stop().await;
}

#[tokio::test]
async fn lan_mode_requires_token_everywhere() {
    let mut s = LiveSettings::default();
    s.mode = TranslationMode::EnToVi;
    let hub = CaptionHub::new(&s, Arc::new(Metrics::new()));
    let weak = OverlayServer::start(ServerOptions { port: 0, allow_lan: true, token: "short".into() }, hub.clone(), hub_control_handler(hub.clone())).await;
    assert!(weak.is_err(), "LAN mode refuses a weak token");

    let (srv, _hub) = start(true).await;
    let port = srv.addr().port();
    let r = request(port, get(port, "/overlay?screen=left", "")).await;
    assert!(r.starts_with("HTTP/1.1 401"), "{r}");
    let r = request(port, get(port, &format!("/overlay?screen=left&token={TOKEN}"), "")).await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    srv.stop().await;
}

#[tokio::test]
async fn pushes_updates_to_multiple_clients_and_restores_on_reconnect() {
    let (srv, hub) = start(false).await;
    let port = srv.addr().port();
    let mut a = Sse::open(port, "left").await;
    let mut b = Sse::open(port, "both").await;
    assert!(a.expect("retry: 1000", 2000).await);
    assert!(a.expect("event: layout", 2000).await);
    assert!(b.expect("event: layout", 2000).await);

    hub.translation_delta(Lang::Vi, "Xin chào quý vị", Instant::now());
    assert!(a.expect("Xin chào quý vị", 2000).await, "client A got the push");
    assert!(b.expect("Xin chào quý vị", 2000).await, "client B got the push");
    // A left-only client never receives right-screen frames.
    hub.translation_delta(Lang::Vi, " và các bạn", Instant::now());
    assert!(a.expect(r#""screen":"left""#, 2000).await);
    assert!(!a.buf.contains(r#""screen":"right""#));

    // Client drops and reconnects: current state arrives immediately.
    drop(a);
    let mut a2 = Sse::open(port, "left").await;
    assert!(a2.expect("Xin chào quý vị và các bạn", 2000).await);
    assert!(wait_clients(&hub, 2).await, "disconnected client was cleaned up");

    // Emergency hide reaches the overlay as visible:false.
    hub.control(live_core::router::Control::Hide);
    assert!(a2.expect(r#""visible":false"#, 2000).await);

    // Overlay latency reports are recorded.
    let body = r#"{"transport":[3,4],"render":[12]}"#;
    let r = request(port, format!("POST /metrics HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n{body}", body.len())).await;
    assert!(r.starts_with("HTTP/1.1 204"), "{r}");
    assert_eq!(hub.metrics.summary(names::OVERLAY_RENDER).unwrap().count, 1);

    // Shutdown closes live streams promptly.
    srv.stop().await;
    assert!(a2.closed_within(2000).await);
    assert!(b.closed_within(2000).await);
}

async fn wait_clients(hub: &Arc<CaptionHub>, n: u64) -> bool {
    for _ in 0..100 {
        if hub.metrics.overlay_clients.load(std::sync::atomic::Ordering::Relaxed) == n {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn throttle_coalesces_updates() {
    let mut s = LiveSettings::default();
    s.mode = TranslationMode::EnToVi;
    s.captions.throttle_ms = 300;
    let hub = CaptionHub::new(&s, Arc::new(Metrics::new()));
    let srv = OverlayServer::start(ServerOptions { port: 0, allow_lan: false, token: TOKEN.into() }, hub.clone(), hub_control_handler(hub.clone())).await.unwrap();
    let port = srv.addr().port();
    let mut c = Sse::open(port, "left").await;
    assert!(c.expect("event: layout", 2000).await);
    assert!(c.expect("event: caption", 2000).await);
    hub.translation_delta(Lang::Vi, "một", Instant::now());
    assert!(c.expect("một", 1000).await);
    // Burst within the throttle window: intermediate frames are skipped and the
    // latest state arrives once the window passes.
    for w in [" hai", " ba", " bốn", " năm"] {
        hub.translation_delta(Lang::Vi, w, Instant::now());
    }
    assert!(c.expect("một hai ba bốn năm", 1500).await);
    assert!(!c.all.contains("\"stable\":\"một hai\""), "intermediate frame leaked: {}", c.all);
    srv.stop().await;
}

#[tokio::test]
async fn port_in_use_is_reported() {
    let (srv, hub) = start(false).await;
    let port = srv.addr().port();
    let second = OverlayServer::start(ServerOptions { port, allow_lan: false, token: TOKEN.into() }, hub.clone(), hub_control_handler(hub.clone())).await;
    assert!(second.err().unwrap().contains("cannot listen"));
    srv.stop().await;
}
