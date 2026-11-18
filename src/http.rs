//! A deliberately small HTTP/1.1 server on `std::net`.
//!
//! Routes: `/` (the page), `/api/state` (everything as JSON), `/api/thread`
//! (one thread's drill-down, built on demand), `/events`
//! (Server-Sent Events), `/healthz`. GET and HEAD only. Requests whose Host
//! header is not localhost are refused, which blocks DNS-rebinding pages
//! from reading the dashboard.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::hub::{Hub, Seen};
use crate::drill;
use crate::payload::Shared;
use crate::timeutil::now_ms;
use std::sync::Mutex;

const MAX_CONNECTIONS: usize = 24;
const HEARTBEAT: Duration = Duration::from_secs(15);
static OPEN: AtomicUsize = AtomicUsize::new(0);

pub enum Ui {
    Embedded(&'static str),
    /// The page read from disk on every request, with `assets/` beside it
    /// (development: edits show up without a rebuild).
    Disk(PathBuf),
}

/// A file compiled into the binary and served under `/assets/<name>`.
pub struct Asset {
    pub name: &'static str,
    pub ctype: &'static str,
    pub bytes: &'static [u8],
}

pub struct Server {
    pub hub: Arc<Hub>,
    pub shared: Arc<Mutex<Shared>>,
    pub t3_db: PathBuf,
    pub ui: Ui,
    pub assets: &'static [Asset],
    pub port: u16,
    /// Extra Host values to accept (the test container publishes a port).
    pub extra_hosts: Vec<String>,
}

const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

fn ctype_of(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "woff2" => "font/woff2",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Asset names are plain relative paths: no "..", no leading slash, no
/// characters outside a small safe set.
fn safe_asset_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.split('/').any(|p| p.is_empty() || p == "." || p == "..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'))
}

fn respond(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8], head: bool) -> std::io::Result<()> {
    respond_with(s, status, ctype, body, head, "no-store")
}

fn respond_with(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8], head: bool, cache: &str) -> std::io::Result<()> {
    let hdr = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: {cache}\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: {CSP}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(hdr.as_bytes())?;
    if !head {
        s.write_all(body)?;
    }
    s.flush()
}

struct Req {
    method: String,
    path: String,
    query: String,
    host: String,
}

fn read_req(s: &mut TcpStream) -> Option<Req> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 16 * 1024 {
            return None;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (path, query) = (path.to_string(), query.to_string());
    let mut host = String::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("host") {
                host = v.trim().to_ascii_lowercase();
            }
        }
    }
    Some(Req { method, path, query, host })
}

impl Server {
    fn host_ok(&self, host: &str) -> bool {
        let p = self.port;
        let ok = [format!("127.0.0.1:{p}"), format!("localhost:{p}"), format!("[::1]:{p}")];
        ok.iter().any(|h| h == host) || self.extra_hosts.iter().any(|h| h == host)
    }

    pub fn run(self: Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming().flatten() {
            if OPEN.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                let mut s = stream;
                let _ = respond(&mut s, "503 Service Unavailable", "text/plain", b"busy", false);
                continue;
            }
            OPEN.fetch_add(1, Ordering::SeqCst);
            let me = self.clone();
            let spawned = std::thread::Builder::new().name("conn".into()).stack_size(256 * 1024).spawn(move || {
                me.handle(stream);
                OPEN.fetch_sub(1, Ordering::SeqCst);
            });
            if spawned.is_err() {
                OPEN.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    fn handle(&self, mut s: TcpStream) {
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
        let _ = s.set_nodelay(true);
        let Some(req) = read_req(&mut s) else { return };
        if !self.host_ok(&req.host) {
            let _ = respond(&mut s, "403 Forbidden", "text/plain", b"forbidden host", false);
            return;
        }
        let head = req.method == "HEAD";
        if req.method != "GET" && !head {
            let _ = respond(&mut s, "405 Method Not Allowed", "text/plain", b"GET only", false);
            return;
        }
        match req.path.as_str() {
            "/" | "/index.html" => {
                let page = match &self.ui {
                    Ui::Embedded(p) => p.as_bytes().to_vec(),
                    Ui::Disk(path) => std::fs::read(path).unwrap_or_else(|e| format!("cannot read UI: {e}").into_bytes()),
                };
                let _ = respond(&mut s, "200 OK", "text/html; charset=utf-8", &page, head);
            }
            "/api/state" => {
                self.refresh_if_stale();
                let (snap, _) = self.hub.snapshot();
                let _ = respond(&mut s, "200 OK", "application/json", snap.as_bytes(), head);
            }
            "/api/thread" => {
                let Some(id) = drill::id_from_query(&req.query) else {
                    let _ = respond(&mut s, "400 Bad Request", "text/plain", b"bad thread id", head);
                    return;
                };
                let _ = match drill::build(&self.shared, &self.t3_db, &id, now_ms()) {
                    Ok(body) => respond(&mut s, "200 OK", "application/json", body.as_bytes(), head),
                    Err(drill::DrillErr::NotFound) => respond(&mut s, "404 Not Found", "text/plain", b"unknown thread", head),
                    Err(drill::DrillErr::Unavailable) => respond(&mut s, "503 Service Unavailable", "text/plain", b"T3 data unavailable", head),
                    Err(drill::DrillErr::Db) => respond(&mut s, "500 Internal Server Error", "text/plain", b"cannot read thread", head),
                };
            }
            "/healthz" => {
                let _ = respond(&mut s, "200 OK", "text/plain", b"ok", head);
            }
            p if p.starts_with("/assets/") => {
                let name = &p["/assets/".len()..];
                let found: Option<(Vec<u8>, &str)> = if !safe_asset_name(name) {
                    None
                } else {
                    match &self.ui {
                        Ui::Disk(page) => {
                            let dir = page.parent().map(|d| d.join("assets")).unwrap_or_else(|| PathBuf::from("assets"));
                            std::fs::read(dir.join(name)).ok().map(|b| (b, ctype_of(name)))
                        }
                        Ui::Embedded(_) => self.assets.iter().find(|a| a.name == name).map(|a| (a.bytes.to_vec(), a.ctype)),
                    }
                };
                let _ = match found {
                    Some((body, ctype)) => respond_with(&mut s, "200 OK", ctype, &body, head, "public, max-age=86400"),
                    None => respond(&mut s, "404 Not Found", "text/plain", b"not found", head),
                };
            }
            "/events" if !head => self.sse(s),
            _ => {
                let _ = respond(&mut s, "404 Not Found", "text/plain", b"not found", head);
            }
        }
    }

    /// Rebuilds the live and days JSON if the workers skipped it while
    /// nobody was connected.
    fn refresh_if_stale(&self) {
        if !self.hub.take_stale() {
            return;
        }
        let now = now_ms();
        let (live, days) = {
            let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            (crate::payload::compose_live(&mut sh, now), crate::payload::compose_days(&sh, now))
        };
        self.hub.set_live_at(live, now);
        self.hub.set_days_at(days, now);
    }

    fn sse(&self, mut s: TcpStream) {
        let hdr = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\nX-Content-Type-Options: nosniff\r\n\r\nretry: 1500\n\n";
        if s.write_all(hdr.as_bytes()).is_err() {
            return;
        }
        let _client = self.hub.client();
        self.refresh_if_stale();
        let (snap, mut seen): (String, Seen) = self.hub.snapshot();
        if write_event(&mut s, "hello", &snap).is_err() {
            return;
        }
        loop {
            let up = self.hub.wait(seen, HEARTBEAT);
            let quiet = up.live.is_none() && up.days.is_none() && up.feed.is_empty();
            let res = if quiet {
                s.write_all(b": ping\n\n").and_then(|_| s.flush())
            } else {
                (|| {
                    if let Some(l) = &up.live {
                        write_event(&mut s, "live", l)?;
                    }
                    if let Some(d) = &up.days {
                        write_event(&mut s, "days", d)?;
                    }
                    if !up.feed.is_empty() {
                        let mut arr = String::with_capacity(up.feed.len() * 200 + 2);
                        arr.push('[');
                        for (i, f) in up.feed.iter().enumerate() {
                            if i > 0 {
                                arr.push(',');
                            }
                            arr.push_str(f);
                        }
                        arr.push(']');
                        write_event(&mut s, "feed", &arr)?;
                    }
                    Ok(())
                })()
            };
            if res.is_err() {
                return;
            }
            seen = up.seen;
        }
    }
}

fn write_event(s: &mut TcpStream, name: &str, data: &str) -> std::io::Result<()> {
    // Payloads are compact JSON (no raw newlines), so one data line is enough.
    s.write_all(b"event: ")?;
    s.write_all(name.as_bytes())?;
    s.write_all(b"\ndata: ")?;
    s.write_all(data.as_bytes())?;
    s.write_all(b"\n\n")?;
    s.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    fn start() -> (u16, Arc<Hub>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let hub = Hub::new();
        hub.set_live("{\"ok\":true}".into());
        static TEST_ASSETS: &[Asset] = &[Asset { name: "fonts/a.woff2", ctype: "font/woff2", bytes: b"wOF2" }];
        let srv = Arc::new(Server {
            hub: hub.clone(),
            shared: Arc::new(Mutex::new(Shared::new(0))),
            t3_db: PathBuf::from("/nonexistent/statev2.sqlite"),
            ui: Ui::Embedded("<!doctype html><title>t</title>"),
            assets: TEST_ASSETS,
            port,
            extra_hosts: vec![],
        });
        std::thread::spawn(move || srv.run(l));
        (port, hub)
    }

    fn get(port: u16, path: &str, host: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn routes_and_host_check() {
        let (port, _) = start();
        let h = format!("127.0.0.1:{port}");
        let state = get(port, "/api/state", &h);
        assert!(state.starts_with("HTTP/1.1 200"));
        assert!(state.contains("{\"live\":{\"ok\":true}"));
        assert!(get(port, "/nope", &h).starts_with("HTTP/1.1 404"));
        // Thread drill-down: bad ids are 400, a good id with no T3 data is 503.
        for bad in ["/api/thread", "/api/thread?id=", "/api/thread?id=a%2Fb", "/api/thread?id=a%20b", "/api/thread?id=..%2f..%2fetc"] {
            assert!(get(port, bad, &h).starts_with("HTTP/1.1 400"), "{bad}");
        }
        assert!(get(port, "/api/thread?id=mcp:abc", &h).starts_with("HTTP/1.1 503"));
        assert!(get(port, "/api/thread?id=mcp:abc", "evil.example:80").starts_with("HTTP/1.1 403"));
        assert!(get(port, "/", "evil.example:80").starts_with("HTTP/1.1 403"));
        assert!(get(port, "/", &format!("localhost:{port}")).contains("<title>t</title>"));
        let font = get(port, "/assets/fonts/a.woff2", &h);
        assert!(font.starts_with("HTTP/1.1 200") && font.contains("font/woff2") && font.ends_with("wOF2"));
        assert!(get(port, "/assets/../Cargo.toml", &h).starts_with("HTTP/1.1 404"));
        assert!(get(port, "/assets/missing.png", &h).starts_with("HTTP/1.1 404"));
        assert!(!safe_asset_name("../x") && !safe_asset_name("/etc/passwd") && !safe_asset_name("a//b") && safe_asset_name("fonts/caveat.woff2"));
    }

    #[test]
    fn sse_starts_with_hello_then_streams() {
        let (port, hub) = start();
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
        let mut r = std::io::BufReader::new(s);
        let mut lines = Vec::new();
        let mut line = String::new();
        while !lines.iter().any(|l: &String| l.starts_with("data: {\"live\"")) {
            line.clear();
            r.read_line(&mut line).unwrap();
            lines.push(line.clone());
        }
        assert!(lines.iter().any(|l| l == "event: hello\n"));
        hub.set_live("{\"n\":2}".into());
        let mut got = false;
        for _ in 0..10 {
            line.clear();
            r.read_line(&mut line).unwrap();
            if line == "data: {\"n\":2}\n" {
                got = true;
                break;
            }
        }
        assert!(got);
    }
}
