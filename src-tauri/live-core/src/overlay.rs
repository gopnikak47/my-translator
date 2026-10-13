//! Local broadcast overlay server (HTTP + Server-Sent Events).
//!
//! Routes
//! * `GET /overlay?screen=left|right|both|canvas` — transparent subtitle page
//! * `GET /overlay.js`                             — page script
//! * `GET /events?screen=…`                        — SSE stream of layout + caption frames
//! * `GET /state?screen=left|right`                — current frame as JSON (diagnostics)
//! * `GET /health`                                 — liveness
//! * `POST /metrics`                               — overlay pages report paint latency
//! * `POST /control/<action>`                      — operator control (token required)
//!
//! Security defaults: binds 127.0.0.1 only; validates the `Host` header (blocks
//! DNS-rebinding) and any `Origin` header (blocks other web pages); sends no CORS
//! headers; never exposes API keys (frames contain caption text only); control
//! endpoints always need the access token, and LAN mode requires it everywhere.
//!
//! SSE is used instead of WebSockets because it is one-directional, reconnects
//! automatically in every Chromium build (vMix and OBS both embed CEF), needs no
//! extra dependency and pushes updates without polling.

use crate::hub::CaptionHub;
use crate::lang::Lang;
use crate::metrics::{names, Basis};
use crate::router::{Control, ScreenFrame, ScreenId};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

const OVERLAY_HTML: &str = include_str!("../assets/overlay.html");
const OVERLAY_JS: &str = include_str!("../assets/overlay.js");

const MAX_CONNECTIONS: usize = 64;
const MAX_HEADER: usize = 8 * 1024;
const MAX_BODY: usize = 32 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT: Duration = Duration::from_secs(15);

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; \
font-src 'self' data:; img-src data:; base-uri 'none'; form-action 'none'";

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub port: u16,
    pub allow_lan: bool,
    pub token: String,
}

/// Operator actions available over HTTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRequest {
    Caption(Control),
    /// Spoken-language override (`None` = automatic detection).
    Language(Option<Lang>),
}

pub fn parse_control(action: &str) -> Option<ControlRequest> {
    Some(match action {
        "hide" => ControlRequest::Caption(Control::Hide),
        "show" => ControlRequest::Caption(Control::Show),
        "toggle" => ControlRequest::Caption(Control::ToggleHidden),
        "freeze" => ControlRequest::Caption(Control::Freeze),
        "unfreeze" => ControlRequest::Caption(Control::Unfreeze),
        "toggle-freeze" => ControlRequest::Caption(Control::ToggleFrozen),
        "clear" => ControlRequest::Caption(Control::Clear),
        "lang-en" => ControlRequest::Language(Some(Lang::En)),
        "lang-vi" => ControlRequest::Language(Some(Lang::Vi)),
        "lang-auto" => ControlRequest::Language(None),
        _ => return None,
    })
}

pub type ControlHandler = Arc<dyn Fn(ControlRequest) + Send + Sync>;

/// Default handler: apply directly to the hub.
pub fn hub_control_handler(hub: Arc<CaptionHub>) -> ControlHandler {
    Arc::new(move |req| match req {
        ControlRequest::Caption(c) => hub.control(c),
        ControlRequest::Language(l) => {
            hub.set_override(l);
        }
    })
}

pub struct OverlayServer {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    opts: ServerOptions,
}

struct Shared {
    hub: Arc<CaptionHub>,
    opts: ServerOptions,
    port: u16,
    control: ControlHandler,
    connections: AtomicUsize,
    shutdown: watch::Receiver<bool>,
}

impl OverlayServer {
    /// Bind and start serving. Must be called inside a Tokio runtime.
    pub async fn start(opts: ServerOptions, hub: Arc<CaptionHub>, control: ControlHandler) -> Result<Self, String> {
        if opts.allow_lan && opts.token.len() < 16 {
            return Err("LAN mode needs an access token of at least 16 characters".into());
        }
        let ip = if opts.allow_lan { [0, 0, 0, 0] } else { [127, 0, 0, 1] };
        let listener = TcpListener::bind(SocketAddr::from((ip, opts.port)))
            .await
            .map_err(|e| format!("cannot listen on port {}: {e}", opts.port))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let (tx, rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            hub,
            opts: opts.clone(),
            port: addr.port(),
            control,
            connections: AtomicUsize::new(0),
            shutdown: rx,
        });
        let task = tokio::spawn(accept_loop(listener, shared));
        Ok(Self { addr, shutdown: tx, task: Some(task), opts })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn options(&self) -> &ServerOptions {
        &self.opts
    }

    /// Stop accepting, end every SSE stream and wait (bounded) for the loop.
    pub async fn stop(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(t) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
        }
    }
}

impl Drop for OverlayServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    let mut shutdown = shared.shutdown.clone();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            res = listener.accept() => {
                let Ok((mut stream, _peer)) = res else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let _ = stream.set_nodelay(true);
                if shared.connections.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    let _ = write_simple(&mut stream, 503, "text/plain", b"busy").await;
                    continue;
                }
                shared.connections.fetch_add(1, Ordering::Relaxed);
                let sh = shared.clone();
                tokio::spawn(async move {
                    let _ = handle(stream, &sh).await;
                    sh.connections.fetch_sub(1, Ordering::Relaxed);
                });
            }
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<Request, u16> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = match tokio::time::timeout(READ_TIMEOUT, stream.read(&mut tmp)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return Err(0),
            Ok(Ok(n)) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = find(&buf, b"\r\n\r\n") {
            if p > MAX_HEADER {
                return Err(431);
            }
            break p;
        }
        if buf.len() > MAX_HEADER {
            return Err(431);
        }
    };
    let head = std::str::from_utf8(&buf[..header_end]).map_err(|_| 400u16)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(400u16)?;
    let mut parts = request_line.split(' ');
    let method = parts.next().ok_or(400u16)?.to_string();
    let target = parts.next().ok_or(400u16)?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target.to_string(), Vec::new()),
    };
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let mut body = buf[header_end + 4..].to_vec();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err(413);
    }
    while body.len() < len {
        let n = match tokio::time::timeout(READ_TIMEOUT, stream.read(&mut tmp)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return Err(400),
            Ok(Ok(n)) => n,
        };
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    Ok(Request { method, path, query, headers, body })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Constant-time token comparison.
fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Shared {
    fn host_allowed(&self, req: &Request) -> bool {
        if self.opts.allow_lan {
            return true; // token check applies instead
        }
        let Some(host) = req.header("host") else { return false };
        let host = host.to_ascii_lowercase();
        let p = self.port;
        host == format!("127.0.0.1:{p}") || host == format!("localhost:{p}") || host == format!("[::1]:{p}")
    }

    fn origin_allowed(&self, req: &Request) -> bool {
        let Some(origin) = req.header("origin") else { return true };
        let o = origin.to_ascii_lowercase();
        if self.opts.allow_lan {
            // Same-origin requests only: Origin must match the Host the page used.
            return req.header("host").map(|h| o == format!("http://{}", h.to_ascii_lowercase())).unwrap_or(false);
        }
        let p = self.port;
        o == format!("http://127.0.0.1:{p}") || o == format!("http://localhost:{p}") || o == format!("http://[::1]:{p}")
    }

    fn token_ok(&self, req: &Request) -> bool {
        if self.opts.token.is_empty() {
            return false;
        }
        let given = req.header("x-access-token").or_else(|| req.param("token")).unwrap_or("");
        token_eq(given, &self.opts.token)
    }
}

async fn handle(mut stream: TcpStream, sh: &Shared) -> std::io::Result<()> {
    let req = match read_request(&mut stream).await {
        Ok(r) => r,
        Err(0) => return Ok(()),
        Err(code) => return write_simple(&mut stream, code, "text/plain", b"bad request").await,
    };
    if !sh.host_allowed(&req) || !sh.origin_allowed(&req) {
        return write_simple(&mut stream, 403, "text/plain", b"forbidden").await;
    }
    if sh.opts.allow_lan && !sh.token_ok(&req) {
        return write_simple(&mut stream, 401, "text/plain", b"token required").await;
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/overlay") | ("GET", "/overlay/") | ("GET", "/") => {
            write_response(&mut stream, 200, "text/html; charset=utf-8", OVERLAY_HTML.as_bytes(), true).await
        }
        ("GET", "/overlay.js") | ("GET", "/overlay/overlay.js") => {
            write_response(&mut stream, 200, "application/javascript; charset=utf-8", OVERLAY_JS.as_bytes(), false).await
        }
        ("GET", "/health") => write_response(&mut stream, 200, "application/json", br#"{"ok":true}"#, false).await,
        ("GET", "/state") => {
            let id = match req.param("screen") {
                Some("right") => ScreenId::Right,
                _ => ScreenId::Left,
            };
            let f = sh.hub.current_frame(id);
            let body = serde_json::to_vec(&*f).unwrap_or_default();
            write_response(&mut stream, 200, "application/json", &body, false).await
        }
        ("GET", "/events") => {
            let screens: Vec<ScreenId> = match req.param("screen").unwrap_or("both") {
                "left" => vec![ScreenId::Left],
                "right" => vec![ScreenId::Right],
                _ => vec![ScreenId::Left, ScreenId::Right],
            };
            sse(stream, sh, screens).await
        }
        ("POST", "/metrics") => {
            record_overlay_metrics(sh, &req.body);
            write_response(&mut stream, 204, "text/plain", b"", false).await
        }
        ("POST", p) if p.starts_with("/control/") => {
            if !sh.token_ok(&req) {
                return write_simple(&mut stream, 401, "text/plain", b"token required").await;
            }
            match parse_control(&p["/control/".len()..]) {
                Some(action) => {
                    (sh.control)(action);
                    write_response(&mut stream, 200, "application/json", br#"{"ok":true}"#, false).await
                }
                None => write_simple(&mut stream, 404, "text/plain", b"unknown action").await,
            }
        }
        ("GET", _) | ("POST", _) => write_simple(&mut stream, 404, "text/plain", b"not found").await,
        _ => write_simple(&mut stream, 405, "text/plain", b"method not allowed").await,
    }
}

fn record_overlay_metrics(sh: &Shared, body: &[u8]) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else { return };
    for (key, name) in [("transport", names::OVERLAY_TRANSPORT), ("render", names::OVERLAY_RENDER)] {
        if let Some(arr) = v.get(key).and_then(|a| a.as_array()) {
            for x in arr.iter().take(200).filter_map(|x| x.as_f64()) {
                sh.hub.metrics.record(name, Basis::Measured, x);
            }
        }
    }
}

fn common_headers(content_type: &str, len: usize, html: bool) -> String {
    let mut h = format!(
        "Content-Type: {content_type}\r\nContent-Length: {len}\r\nCache-Control: no-store\r\n\
X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n"
    );
    if html {
        h.push_str(&format!("Content-Security-Policy: {CSP}\r\n"));
    }
    h
}

async fn write_response(stream: &mut TcpStream, code: u16, ct: &str, body: &[u8], html: bool) -> std::io::Result<()> {
    let head = format!("HTTP/1.1 {code} {}\r\n{}\r\n", reason(code), common_headers(ct, body.len(), html));
    let mut out = Vec::with_capacity(head.len() + body.len());
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(body);
    let _ = tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(&out)).await;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn write_simple(stream: &mut TcpStream, code: u16, ct: &str, body: &[u8]) -> std::io::Result<()> {
    write_response(stream, code, ct, body, false).await
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn frame_event(f: &ScreenFrame) -> String {
    format!("event: caption\ndata: {}\n\n", serde_json::to_string(f).unwrap_or_default())
}

async fn send<W: AsyncWriteExt + Unpin>(stream: &mut W, data: &str) -> bool {
    matches!(tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(data.as_bytes())).await, Ok(Ok(())))
}

async fn sse(stream: TcpStream, sh: &Shared, screens: Vec<ScreenId>) -> std::io::Result<()> {
    // Read half is only watched for EOF so a closed client is released at once
    // (instead of at the next heartbeat write).
    let (mut rd, mut stream) = stream.into_split();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-store\r\n\
X-Content-Type-Options: nosniff\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n";
    let mut layout_rx = sh.hub.subscribe_layout();
    let mut rxs: Vec<_> = screens.iter().map(|s| sh.hub.subscribe_screen(*s)).collect();
    let mut shutdown = sh.shutdown.clone();
    sh.hub.metrics.overlay_clients.fetch_add(1, Ordering::Relaxed);
    let _guard = ClientGuard(&sh.hub);

    // Initial state: reconnecting clients are fully restored immediately.
    let mut first = String::from(head);
    first.push_str("retry: 1000\n\n");
    let layout = layout_rx.borrow_and_update().clone();
    first.push_str(&format!("event: layout\ndata: {}\n\n", serde_json::to_string(&*layout).unwrap_or_default()));
    for rx in rxs.iter_mut() {
        let f = rx.borrow_and_update().clone();
        first.push_str(&frame_event(&f));
    }
    if !send(&mut stream, &first).await {
        return Ok(());
    }

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    loop {
        // Wait for any screen to change (or layout / heartbeat / shutdown).
        let changed = async {
            match rxs.len() {
                1 => rxs[0].changed().await.is_ok(),
                _ => {
                    let (a, b) = rxs.split_at_mut(1);
                    tokio::select! {
                        r = a[0].changed() => r.is_ok(),
                        r = b[0].changed() => r.is_ok(),
                    }
                }
            }
        };
        let mut sink = [0u8; 256];
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            r = rd.read(&mut sink) => {
                if matches!(r, Ok(0) | Err(_)) {
                    return Ok(());
                }
            }
            ok = changed => {
                if !ok {
                    return Ok(());
                }
                let mut out = String::new();
                for rx in rxs.iter_mut() {
                    if rx.has_changed().unwrap_or(false) || out.is_empty() {
                        let f = rx.borrow_and_update().clone();
                        out.push_str(&frame_event(&f));
                    }
                }
                if !send(&mut stream, &out).await {
                    return Ok(());
                }
                let throttle = sh.hub.throttle();
                if !throttle.is_zero() {
                    tokio::time::sleep(throttle).await;
                }
            }
            r = layout_rx.changed() => {
                if r.is_err() {
                    return Ok(());
                }
                let l = layout_rx.borrow_and_update().clone();
                if !send(&mut stream, &format!("event: layout\ndata: {}\n\n", serde_json::to_string(&*l).unwrap_or_default())).await {
                    return Ok(());
                }
            }
            _ = heartbeat.tick() => {
                if !send(&mut stream, ": hb\n\n").await {
                    return Ok(());
                }
            }
        }
    }
}

struct ClientGuard<'a>(&'a Arc<CaptionHub>);

impl Drop for ClientGuard<'_> {
    fn drop(&mut self) {
        self.0.metrics.overlay_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_percent_decoding() {
        let q = parse_query("screen=left&token=a%2Bb+c&x");
        assert_eq!(q[0], ("screen".into(), "left".into()));
        assert_eq!(q[1], ("token".into(), "a+b c".into()));
        assert_eq!(q[2], ("x".into(), "".into()));
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn control_actions() {
        assert_eq!(parse_control("hide"), Some(ControlRequest::Caption(Control::Hide)));
        assert_eq!(parse_control("lang-vi"), Some(ControlRequest::Language(Some(Lang::Vi))));
        assert_eq!(parse_control("lang-auto"), Some(ControlRequest::Language(None)));
        assert_eq!(parse_control("rm -rf"), None);
    }

    #[test]
    fn constant_time_eq() {
        assert!(token_eq("abc", "abc"));
        assert!(!token_eq("abc", "abd"));
        assert!(!token_eq("abc", "abcd"));
    }
}
