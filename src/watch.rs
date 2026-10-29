//! The watchdog: a small separate service (`cookie-watch`) that always knows whether cookie-web is up.
//!
//! - cookie-web **registers** with it on start (pid, version, port) and sends a **heartbeat** every
//!   few seconds; on a clean shutdown it says **bye**;
//! - the watchdog also checks on its own: does that process still exist, and does `/api/health`
//!   answer? (A process can exist and still be stuck — that is "unresponsive".)
//! - it keeps running when cookie-web is down, so `GET /watch/status` always answers:
//!   `online`, `degraded` (up, but the model isn't ready), `unresponsive`, `offline` or `unknown`.
//!
//! Behind nginx the watchdog is reached at `/watch/…`; when cookie-web is down nginx sends pages to
//! the watchdog's status page instead of an error.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{ConnectInfo, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::Auth;

/// How often cookie-web sends a heartbeat.
pub const BEAT_EVERY: Duration = Duration::from_secs(5);

/// What cookie-web tells the watchdog about itself.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Beat {
    pub pid: u32,
    /// The process name (`/proc/<pid>/comm`), so a reused pid isn't mistaken for cookie-web.
    #[serde(default)]
    pub name: Option<String>,
    pub version: String,
    pub port: u16,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub jobs_running: usize,
    #[serde(default)]
    pub jobs_queued: usize,
    #[serde(default)]
    pub jobs_paused: usize,
}

impl Beat {
    /// This process's name, as the watchdog will see it.
    pub fn own_name() -> Option<String> {
        std::fs::read_to_string("/proc/self/comm").ok().map(|s| s.trim().to_string())
    }
}

// ------------------------------------------------------------------ client side (cookie-web)

/// Send a heartbeat every [`BEAT_EVERY`] until the process ends. Failures are ignored (the
/// watchdog may not be installed, or may be restarting): it notices the gap on its own.
pub fn start_heartbeat(url: String, beat: impl Fn() -> Beat + Send + Sync + 'static) {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(3)).build().unwrap_or_default();
    tokio::spawn(async move {
        let mut warned = false;
        loop {
            match http.post(format!("{url}/watch/beat")).json(&beat()).send().await {
                Ok(r) if r.status().is_success() => warned = false,
                Ok(r) if !warned => {
                    tracing::warn!("watchdog at {url} refused the heartbeat ({})", r.status());
                    warned = true;
                }
                Err(e) if !warned => {
                    tracing::warn!("watchdog at {url} is not reachable ({e}); retrying quietly");
                    warned = true;
                }
                _ => {}
            }
            tokio::time::sleep(BEAT_EVERY).await;
        }
    });
}

/// Tell the watchdog this is a clean shutdown (best effort, short timeout).
pub async fn say_bye(url: &str) {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(2)).build().unwrap_or_default();
    let _ = http.post(format!("{url}/watch/bye")).json(&json!({ "pid": std::process::id() })).send().await;
}

// ------------------------------------------------------------------ systemd

/// Send a message to systemd (`READY=1`, `WATCHDOG=1`, `STOPPING=1`) when running under a
/// `Type=notify` unit. Does nothing elsewhere.
pub fn sd_notify(msg: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::{linux::net::SocketAddrExt, unix::net::UnixDatagram};
        let Some(path) = std::env::var_os("NOTIFY_SOCKET") else { return false };
        let Ok(sock) = UnixDatagram::unbound() else { return false };
        let path = path.to_string_lossy().into_owned();
        let sent = match path.strip_prefix('@') {
            Some(name) => std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
                .and_then(|addr| sock.send_to_addr(msg.as_bytes(), &addr)),
            None => sock.send_to(msg.as_bytes(), &path),
        };
        sent.is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = msg;
        false
    }
}

/// systemd's own watchdog (`WatchdogSec=` in the unit): ping it only while the server really
/// answers HTTP, so a hung process is restarted by systemd — not just a crashed one.
pub fn start_systemd_watchdog(port: u16) {
    let Some(usec) = std::env::var("WATCHDOG_USEC").ok().and_then(|v| v.parse::<u64>().ok()) else { return };
    let every = Duration::from_micros(usec / 3).max(Duration::from_secs(1));
    let http = reqwest::Client::builder().timeout(every).build().unwrap_or_default();
    tracing::info!("systemd watchdog on: checking the server every {}s", every.as_secs());
    tokio::spawn(async move {
        loop {
            let alive = http
                .get(format!("http://127.0.0.1:{port}/api/manifest"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            if alive {
                sd_notify("WATCHDOG=1");
            } else {
                tracing::warn!("self-check failed: the server isn't answering on port {port}");
            }
            tokio::time::sleep(every).await;
        }
    });
}

// ------------------------------------------------------------------ the watchdog

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Nothing heard yet.
    Unknown,
    /// Up and healthy.
    Online,
    /// Up, answering, but not fully working (e.g. the model isn't loaded / Ollama is down).
    Degraded,
    /// The process exists but doesn't answer.
    Unresponsive,
    /// Not running.
    Offline,
}

impl Status {
    /// Is the API answering requests?
    pub fn api_up(self) -> bool {
        matches!(self, Status::Online | Status::Degraded)
    }
}

pub struct WatchConfig {
    /// cookie-web's base URL, for the health probe (e.g. `http://127.0.0.1:8787`).
    pub target: String,
    pub api_key: Option<String>,
    pub password: Option<String>,
    /// Time between checks.
    pub interval: Duration,
}

#[derive(Default, Clone)]
struct Probe {
    ok: bool,
    at: Option<DateTime<Utc>>,
    last_ok_at: Option<DateTime<Utc>>,
    latency_ms: Option<u64>,
    /// `ok` / `degraded` as cookie-web reports it.
    health: Option<String>,
    version: Option<String>,
    error: Option<String>,
    failures: u32,
}

#[derive(Serialize, Clone)]
struct Incident {
    at: DateTime<Utc>,
    from: Status,
    to: Status,
    reason: String,
}

struct WatchState {
    beat: Option<(Beat, Instant, DateTime<Utc>)>,
    bye: bool,
    probe: Probe,
    pid_alive: Option<bool>,
    status: Status,
    reason: String,
    since: DateTime<Utc>,
    incidents: VecDeque<Incident>,
    checks: u64,
}

pub struct Watch {
    cfg: WatchConfig,
    auth: Auth,
    http: reqwest::Client,
    started: Instant,
    state: Mutex<WatchState>,
}

const MAX_INCIDENTS: usize = 50;
/// Consecutive failed health checks before a running process counts as unresponsive.
const FAILS_BEFORE_DOWN: u32 = 2;

/// Is the process alive (and, if `name` is given, still the same program)? `None` = can't tell here.
pub fn pid_alive(pid: u32, name: Option<&str>) -> Option<bool> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return Some(false) };
    // the state letter follows the parenthesised name: "123 (cookie-web) S …"
    let state = stat.rsplit_once(") ").and_then(|(_, rest)| rest.chars().next());
    if matches!(state, Some('Z' | 'X')) {
        return Some(false);
    }
    if let Some(want) = name
        && let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        && comm.trim() != want
    {
        return Some(false); // the pid now belongs to another program
    }
    Some(true)
}

impl Watch {
    pub fn new(cfg: WatchConfig) -> Arc<Self> {
        let auth = Auth::new(cfg.api_key.clone(), cfg.password.clone());
        Arc::new(Self {
            auth,
            http: reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap_or_default(),
            started: Instant::now(),
            state: Mutex::new(WatchState {
                beat: None,
                bye: false,
                probe: Probe::default(),
                pid_alive: None,
                status: Status::Unknown,
                reason: "waiting for the first check".into(),
                since: Utc::now(),
                incidents: VecDeque::new(),
                checks: 0,
            }),
            cfg,
        })
    }

    /// Check every `interval`, forever.
    pub fn start(self: &Arc<Self>) {
        let w = self.clone();
        tokio::spawn(async move {
            loop {
                w.check_now().await;
                tokio::time::sleep(w.cfg.interval).await;
            }
        });
    }

    /// Probe cookie-web and update the status.
    pub async fn check_now(&self) {
        let t = Instant::now();
        let mut req = self.http.get(format!("{}/api/health", self.cfg.target));
        if let Some(k) = &self.cfg.api_key {
            req = req.bearer_auth(k);
        }
        let result = match req.send().await {
            Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                Ok(v) => Ok(v),
                Err(e) => Err(format!("bad health response: {e}")),
            },
            Ok(r) => Err(format!("health check returned {}", r.status())),
            Err(e) if e.is_timeout() => Err("health check timed out".into()),
            Err(e) if e.is_connect() => Err("connection refused".into()),
            Err(e) => Err(format!("health check failed: {e}")),
        };
        let now = Utc::now();
        let mut s = self.state.lock().unwrap();
        s.checks += 1;
        let p = &mut s.probe;
        p.at = Some(now);
        match result {
            Ok(v) => {
                p.ok = true;
                p.last_ok_at = Some(now);
                p.latency_ms = Some(t.elapsed().as_millis() as u64);
                p.health = v["status"].as_str().map(String::from);
                p.version = v["version"].as_str().map(String::from);
                p.error = None;
                p.failures = 0;
            }
            Err(e) => {
                p.ok = false;
                p.latency_ms = None;
                p.error = Some(e);
                p.failures += 1;
            }
        }
        Self::evaluate(&mut s);
    }

    fn evaluate(s: &mut WatchState) {
        let beat = s.beat.as_ref().map(|(b, at, _)| (b.pid, b.name.clone(), at.elapsed()));
        s.pid_alive = beat.as_ref().and_then(|(pid, name, _)| pid_alive(*pid, name.as_deref()));
        let p = &s.probe;
        let (status, reason) = if s.bye {
            // it said it is stopping (it may still be finishing open streams): a new heartbeat clears this
            (Status::Offline, "stopped (clean shutdown)".to_string())
        } else if p.ok {
            if p.health.as_deref() == Some("degraded") {
                (
                    Status::Degraded,
                    "running, but the model isn't ready (is Ollama up and the model pulled?)".to_string(),
                )
            } else {
                (Status::Online, String::new())
            }
        } else if s.pid_alive == Some(false) {
            let pid = beat.as_ref().map(|b| b.0).unwrap_or_default();
            (Status::Offline, format!("the process (pid {pid}) is gone — it exited or crashed"))
        } else if p.failures < FAILS_BEFORE_DOWN && s.status.api_up() {
            // one missed check: keep the status rather than flap
            (s.status, s.reason.clone())
        } else if p.at.is_none() {
            (Status::Unknown, "registered; checking its health".to_string())
        } else if s.pid_alive == Some(true) {
            let err = p.error.clone().unwrap_or_default();
            (Status::Unresponsive, format!("the process is running but doesn't answer ({err})"))
        } else if s.beat.is_none() && p.at.is_some() {
            let err = p.error.clone().unwrap_or_default();
            (Status::Offline, format!("not running ({err}) and it hasn't registered"))
        } else {
            let err = p.error.clone().unwrap_or_default();
            (Status::Offline, format!("not answering ({err})"))
        };
        if status != s.status {
            let at = Utc::now();
            let why = if reason.is_empty() { "healthy".to_string() } else { reason.clone() };
            tracing::info!("cookie-web: {:?} → {:?} ({why})", s.status, status);
            s.incidents.push_front(Incident { at, from: s.status, to: status, reason: why });
            s.incidents.truncate(MAX_INCIDENTS);
            s.status = status;
            s.since = at;
        }
        s.reason = reason;
    }

    /// A heartbeat (also the registration: the first one, or one with a new pid).
    /// Returns true for a (re)registration.
    pub fn beat(&self, b: Beat) -> bool {
        let mut s = self.state.lock().unwrap();
        let new = s.beat.as_ref().is_none_or(|(old, _, _)| old.pid != b.pid);
        if new {
            tracing::info!("cookie-web {} registered (pid {}, port {})", b.version, b.pid, b.port);
        }
        let registered = if new { Utc::now() } else { s.beat.as_ref().map(|x| x.2).unwrap_or_else(Utc::now) };
        s.beat = Some((b, Instant::now(), registered));
        s.bye = false;
        if new {
            // a fresh process: forget the old one's failed checks; its status is decided by the health
            // check that the heartbeat handler runs right away (no "unresponsive" blip in between)
            s.probe.failures = 0;
            s.reason = "registered; checking its health".into();
        } else {
            Self::evaluate(&mut s);
        }
        new
    }

    /// A clean shutdown.
    pub fn bye(&self, pid: u32) {
        let mut s = self.state.lock().unwrap();
        if s.beat.as_ref().is_none_or(|(b, _, _)| b.pid == pid) {
            tracing::info!("cookie-web (pid {pid}) is shutting down");
            s.bye = true;
            s.probe.ok = false;
            s.probe.failures = FAILS_BEFORE_DOWN;
            s.probe.error = Some("shut down".into());
            Self::evaluate(&mut s);
        }
    }

    pub fn status(&self) -> Status {
        self.state.lock().unwrap().status
    }

    /// The status document. `detail` adds the process details and the incident log.
    pub fn report(&self, detail: bool) -> Value {
        let s = self.state.lock().unwrap();
        let mut v = json!({
            "service": "cookie-web",
            "status": s.status,
            "ok": s.status == Status::Online,
            "api": if s.status.api_up() { "up" } else { "down" },
            "reason": s.reason,
            "since": s.since,
            "checked_at": s.probe.at,
            "last_ok_at": s.probe.last_ok_at,
            "watchdog": { "uptime_secs": self.started.elapsed().as_secs(), "interval_secs": self.cfg.interval.as_secs() },
        });
        if detail {
            v["detail"] = json!({
                "pid": s.beat.as_ref().map(|b| b.0.pid),
                "pid_alive": s.pid_alive,
                "version": s.beat.as_ref().map(|b| b.0.version.clone()).or(s.probe.version.clone()),
                "port": s.beat.as_ref().map(|b| b.0.port),
                "started_at": s.beat.as_ref().map(|b| b.0.started_at),
                "registered_at": s.beat.as_ref().map(|b| b.2),
                "last_beat_secs_ago": s.beat.as_ref().map(|b| b.1.elapsed().as_secs()),
                "jobs_running": s.beat.as_ref().map(|b| b.0.jobs_running),
                "jobs_queued": s.beat.as_ref().map(|b| b.0.jobs_queued),
                "jobs_paused": s.beat.as_ref().map(|b| b.0.jobs_paused),
                "health": s.probe.health,
                "latency_ms": s.probe.latency_ms,
                "error": s.probe.error,
                "failed_checks": s.probe.failures,
                "checks": s.checks,
                "target": self.cfg.target,
            });
            v["incidents"] = json!(s.incidents);
        }
        v
    }
}

// ------------------------------------------------------------------ HTTP

/// Heartbeats only from this machine, and never through the reverse proxy.
fn local_only(peer: Option<SocketAddr>, headers: &HeaderMap) -> bool {
    peer.is_some_and(|p| p.ip().is_loopback())
        && !headers.contains_key("x-forwarded-for")
        && !headers.contains_key("x-real-ip")
        && !headers.contains_key("forwarded")
}

fn signed_in(w: &Watch, headers: &HeaderMap, query_key: Option<&str>) -> bool {
    if !w.auth.required() {
        return true;
    }
    let bearer =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let xkey = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    if bearer.or(xkey).or(query_key).is_some_and(|k| w.auth.check_key(k)) {
        return true;
    }
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(Auth::session_from)
        .any(|t| w.auth.valid_session(t))
}

#[derive(Deserialize)]
struct StatusQ {
    api_key: Option<String>,
}

async fn status(State(w): State<Arc<Watch>>, Query(q): Query<StatusQ>, headers: HeaderMap) -> Response {
    let detail = signed_in(&w, &headers, q.api_key.as_deref());
    let code = if w.status().api_up() { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, [(header::CACHE_CONTROL, "no-store")], Json(w.report(detail))).into_response()
}

fn peer_of(req: &Request) -> Option<SocketAddr> {
    req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0)
}

async fn beat(State(w): State<Arc<Watch>>, req: Request) -> Response {
    if !local_only(peer_of(&req), req.headers()) {
        return (StatusCode::FORBIDDEN, "heartbeats are accepted from this machine only").into_response();
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), 16 * 1024).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    match serde_json::from_slice::<Beat>(&body) {
        Ok(b) => {
            if w.beat(b) {
                let w = w.clone();
                tokio::spawn(async move { w.check_now().await });
            }
            Json(json!({ "ok": true, "every_secs": BEAT_EVERY.as_secs() })).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, format!("bad heartbeat: {e}")).into_response(),
    }
}

async fn bye(State(w): State<Arc<Watch>>, req: Request) -> Response {
    if !local_only(peer_of(&req), req.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), 4096).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let pid = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["pid"].as_u64()).unwrap_or(0) as u32;
    w.bye(pid);
    Json(json!({ "ok": true })).into_response()
}

pub fn router(w: Arc<Watch>) -> Router {
    Router::new()
        .route("/watch", get(|| async { Html(include_str!("../static/watch.html")) }))
        .route("/watch/", get(|| async { Html(include_str!("../static/watch.html")) }))
        .route("/watch/status", get(status))
        .route("/watch/alive", get(|| async { "ok" }))
        .route("/watch/beat", post(beat))
        .route("/watch/bye", post(bye))
        .route("/", get(|| async { (StatusCode::SEE_OTHER, [(header::LOCATION, "/watch/")]) }))
        // other origins (a dashboard, the console client) may read the status
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods([axum::http::Method::GET])
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(w)
}

/// Run the watchdog: `COOKIE_WATCH_HOST`:`COOKIE_WATCH_PORT` (127.0.0.1:8790), watching cookie-web
/// at `COOKIE_WATCH_TARGET` (default `http://127.0.0.1:$COOKIE_PORT`).
pub async fn serve() -> anyhow::Result<()> {
    let cfg = crate::config::Config::from_env();
    let host = std::env::var("COOKIE_WATCH_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = std::env::var("COOKIE_WATCH_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8790);
    let target = std::env::var("COOKIE_WATCH_TARGET")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| format!("http://127.0.0.1:{}", cfg.port))
        .trim_end_matches('/')
        .to_string();
    let interval = std::env::var("COOKIE_WATCH_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(|s: u64| Duration::from_secs(s.clamp(1, 300)))
        .unwrap_or(Duration::from_secs(5));
    let w = Watch::new(WatchConfig { target: target.clone(), api_key: cfg.api_key, password: cfg.password, interval });
    w.start();
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on {addr}: {e} (set COOKIE_WATCH_PORT)"))?;
    tracing::info!("cookie-watch listening on http://{addr}, watching {target} every {}s", interval.as_secs());
    axum::serve(listener, router(w).into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive_and_name_is_checked() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let me = std::process::id();
        assert_eq!(pid_alive(me, None), Some(true));
        assert_eq!(pid_alive(me, Beat::own_name().as_deref()), Some(true));
        assert_eq!(pid_alive(me, Some("definitely-not-this")), Some(false));
        assert_eq!(pid_alive(u32::MAX - 1, None), Some(false));
    }
}
