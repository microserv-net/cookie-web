//! HTTP API + embedded GUI. Every request runs as a *job*; the sync, SSE and
//! async (poll) endpoints are three views of the same job.

use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tower_http::cors::{Any, CorsLayer};

use crate::{
    auth::Auth,
    control::{Control, PauseState},
    depth::Depth,
    engine::{Engine, ResearchRequest, ResearchResult},
    events::{Event, Events},
    queue::Queue,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    /// Paused by the caller: it holds no slot (another job may run) until resumed.
    Paused,
    Done,
    Error,
    Cancelled,
}

pub struct Job {
    pub id: String,
    pub query: String,
    pub created: Instant,
    pub created_at: chrono::DateTime<chrono::Utc>,
    status: Mutex<JobStatus>,
    events: Mutex<Vec<Event>>,
    result: Mutex<Option<ResearchResult>>,
    error: Mutex<Option<String>>,
    tick: watch::Sender<usize>,
    /// Aborts the running task (`DELETE /api/jobs/{id}`).
    abort: Mutex<Option<tokio::task::AbortHandle>>,
    /// The caller's `Idempotency-Key`, if any.
    key: Option<String>,
    /// Live progress, for the status view.
    progress: Mutex<Progress>,
    /// Pause / resume.
    pub control: Arc<Control>,
    queue: Arc<Queue>,
}

/// What a job is doing right now (kept up to date from its events).
#[derive(Default)]
struct Progress {
    stage: Option<String>,
    depth: Option<String>,
    started: Option<Instant>,
    ended: Option<Instant>,
    tool_calls: usize,
    sources: usize,
    tokens: usize,
}

impl Job {
    fn new(query: &str, key: Option<String>, queue: Arc<Queue>) -> Arc<Self> {
        let (tick, _) = watch::channel(0);
        let id = format!("job_{}", uuid::Uuid::new_v4().simple());
        Arc::new(Self {
            control: Control::new(&id, Some(queue.clone())),
            queue,
            id,
            query: query.to_string(),
            created: Instant::now(),
            created_at: chrono::Utc::now(),
            status: Mutex::new(JobStatus::Queued),
            events: Mutex::new(vec![]),
            result: Mutex::new(None),
            error: Mutex::new(None),
            tick,
            abort: Mutex::new(None),
            key,
            progress: Mutex::new(Progress::default()),
        })
    }

    fn mark_running(&self) {
        self.progress.lock().unwrap().started.get_or_insert_with(Instant::now);
    }

    pub fn status(&self) -> JobStatus {
        let st = *self.status.lock().unwrap();
        if st == JobStatus::Running && matches!(self.control.state(), PauseState::Paused | PauseState::Resuming) {
            return JobStatus::Paused;
        }
        st
    }

    fn finished(&self) -> bool {
        matches!(self.status(), JobStatus::Done | JobStatus::Error | JobStatus::Cancelled)
    }

    /// Stop the job if it is still queued, running or paused. Returns false if it had already finished.
    pub fn cancel(&self) -> bool {
        {
            let mut st = self.status.lock().unwrap();
            if matches!(*st, JobStatus::Done | JobStatus::Error | JobStatus::Cancelled) {
                return false;
            }
            *st = JobStatus::Cancelled;
        }
        *self.error.lock().unwrap() = Some("cancelled".into());
        self.progress.lock().unwrap().ended.get_or_insert_with(Instant::now);
        if let Some(h) = self.abort.lock().unwrap().as_ref() {
            h.abort();
        }
        // free its place at once: out of the line, and its slot to the next job
        self.queue.remove(&self.id);
        self.control.release_slot();
        self.tick.send_modify(|_| {});
        true
    }

    /// Pause a running job: the model call in progress stops right away, the job gives up its slot.
    pub fn pause(&self) -> Result<(), String> {
        match self.status() {
            JobStatus::Running => {
                self.control.pause();
                self.tick.send_modify(|_| {});
                Ok(())
            }
            JobStatus::Paused => Ok(()),
            JobStatus::Queued => Err("the job hasn't started yet (it is queued); cancel it instead".into()),
            _ => Err("the job has already finished".into()),
        }
    }

    /// Resume a paused job: it goes to the front of the queue and carries on where it stopped.
    pub fn resume(&self) -> Result<(), String> {
        match self.status() {
            JobStatus::Paused | JobStatus::Running => {
                self.control.resume();
                self.tick.send_modify(|_| {});
                Ok(())
            }
            JobStatus::Queued => Ok(()),
            _ => Err("the job has already finished".into()),
        }
    }

    fn push(&self, e: Event) {
        {
            let mut p = self.progress.lock().unwrap();
            match &e {
                Event::Status { message } => p.stage = Some(message.clone()),
                Event::Triage(t) => p.depth = Some(t.depth.as_str().to_string()),
                Event::ToolCall { tool, .. } => {
                    p.tool_calls += 1;
                    p.stage = Some(format!("Running {tool}"));
                }
                Event::Source { .. } => p.sources += 1,
                Event::Token { .. } => {
                    p.tokens += 1;
                    if p.tokens == 1 {
                        p.stage = Some("Writing the answer".into());
                    }
                }
                Event::Reset { .. } => p.tokens = 0,
                Event::Done { .. } | Event::Error { .. } => {
                    p.ended.get_or_insert_with(Instant::now);
                }
                _ => {}
            }
        }
        match &e {
            Event::Done { result } => {
                *self.result.lock().unwrap() = Some((**result).clone());
            }
            Event::Error { message } => {
                *self.error.lock().unwrap() = Some(message.clone());
            }
            _ => {}
        }
        let finished_now = match &e {
            Event::Done { .. } => Some(JobStatus::Done),
            Event::Error { .. } => Some(JobStatus::Error),
            _ => None,
        };
        let len = {
            let mut ev = self.events.lock().unwrap();
            ev.push(e);
            ev.len()
        };
        if let Some(s) = finished_now {
            let mut st = self.status.lock().unwrap();
            if *st != JobStatus::Cancelled {
                *st = s;
            }
        }
        self.tick.send_replace(len);
    }

    fn snapshot(&self, include_result: bool) -> Value {
        let mut v = json!({
            "id": self.id,
            "query": self.query,
            "status": self.status(),
            "pause": self.control.state(),
            "created_at": self.created_at,
            "events": self.events.lock().unwrap().len(),
            "events_url": format!("/api/jobs/{}/events", self.id),
        });
        {
            let p = self.progress.lock().unwrap();
            let end = p.ended.unwrap_or_else(Instant::now);
            v["stage"] = json!(p.stage);
            v["depth"] = json!(p.depth);
            v["tool_calls"] = json!(p.tool_calls);
            v["sources"] = json!(p.sources);
            // time waiting in the queue, and time running (so far, or in total once finished)
            v["queued_ms"] = json!(p.started.unwrap_or(end).saturating_duration_since(self.created).as_millis() as u64);
            // working time: pauses don't count
            let paused = self.control.paused_for();
            v["running_ms"] = json!(
                p.started
                    .map(|s| end.saturating_duration_since(s).saturating_sub(paused).as_millis() as u64)
                    .unwrap_or(0)
            );
            v["paused_ms"] = json!(paused.as_millis() as u64);
        }
        if let Some(pos) = self.queue.position(&self.id) {
            v["position"] = json!(pos);
        }
        if let Some(r) = self.result.lock().unwrap().as_ref() {
            v["report_id"] = json!(r.id);
            v["degraded"] = json!(r.degraded);
            v["warnings"] = json!(r.warnings.len());
        }
        if include_result && let Some(r) = self.result.lock().unwrap().as_ref() {
            v["result"] = serde_json::to_value(r).unwrap_or(Value::Null);
        }
        if let Some(e) = self.error.lock().unwrap().as_ref() {
            v["error"] = json!(e);
        }
        v
    }

    async fn wait(&self) {
        let mut rx = self.tick.subscribe();
        while !self.finished() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub started: Instant,
    pub engine: Arc<Engine>,
    pub jobs: Arc<Mutex<HashMap<String, Arc<Job>>>>,
    /// Jobs take turns: `COOKIE_MAX_CONCURRENT` at a time, the rest wait in line.
    pub queue: Arc<Queue>,
    pub auth: Arc<Auth>,
    /// Recent working times per depth (ms), for the latency figures in the manifest.
    recent: Arc<Mutex<HashMap<String, VecDeque<u64>>>>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Model and watchdog info for `/api/status`, refreshed at most every few seconds (it is polled).
    status_cache: Arc<tokio::sync::Mutex<StatusCache>>,
}

#[derive(Default)]
struct StatusCache {
    model: Option<(Instant, crate::llm::ModelInfo)>,
    watch: Option<(Instant, Value)>,
}

/// How many recent jobs per depth the latency figures are based on.
const LATENCY_WINDOW: usize = 20;

impl AppState {
    pub fn new(engine: Arc<Engine>) -> Self {
        let queue = Queue::new(engine.cfg.max_concurrent);
        let auth = Arc::new(Auth::new(engine.cfg.api_key.clone(), engine.cfg.password.clone()));
        Self {
            started: Instant::now(),
            started_at: chrono::Utc::now(),
            engine,
            jobs: Arc::new(Mutex::new(HashMap::new())),
            queue,
            auth,
            recent: Arc::new(Mutex::new(HashMap::new())),
            status_cache: Arc::new(tokio::sync::Mutex::new(StatusCache::default())),
        }
    }

    fn count(&self, status: JobStatus) -> usize {
        self.jobs.lock().unwrap().values().filter(|j| j.status() == status).count()
    }

    fn record_latency(&self, depth: &str, ms: u64) {
        let mut r = self.recent.lock().unwrap();
        let q = r.entry(depth.to_string()).or_default();
        q.push_back(ms);
        while q.len() > LATENCY_WINDOW {
            q.pop_front();
        }
    }

    /// Median and slowest-of-90% working time per depth, over the recent jobs.
    fn latency(&self) -> Value {
        let r = self.recent.lock().unwrap();
        let mut out = serde_json::Map::new();
        for d in ["quick", "standard", "deep"] {
            let Some(q) = r.get(d).filter(|q| !q.is_empty()) else { continue };
            let mut v: Vec<u64> = q.iter().copied().collect();
            v.sort_unstable();
            let at = |f: f64| v[((v.len() - 1) as f64 * f).round() as usize];
            out.insert(d.into(), json!({ "jobs": v.len(), "median_ms": at(0.5), "p90_ms": at(0.9) }));
        }
        Value::Object(out)
    }

    /// Start a research job in the background. With an idempotency `key`, a request that repeats
    /// the key of a job still queued, running or done returns that job instead of starting another.
    pub fn spawn(&self, req: ResearchRequest, key: Option<String>) -> Result<Arc<Job>, ApiError> {
        if let Some(k) = key.as_deref() {
            let existing = self
                .jobs
                .lock()
                .unwrap()
                .values()
                .find(|j| {
                    j.key.as_deref() == Some(k)
                        && matches!(
                            j.status(),
                            JobStatus::Queued | JobStatus::Running | JobStatus::Paused | JobStatus::Done
                        )
                })
                .cloned();
            if let Some(j) = existing {
                return Ok(j);
            }
        }
        let cap = self.engine.cfg.max_queue;
        if self.queue.running() >= self.queue.capacity() && self.queue.waiting() >= cap {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("the queue is full ({cap} jobs waiting); try again later"),
            ));
        }
        let job = Job::new(req.query.trim(), key, self.queue.clone());
        self.jobs.lock().unwrap().insert(job.id.clone(), job.clone());
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
        let events = Events::new(tx);
        events.emit(Event::Started { job_id: job.id.clone(), query: job.query.clone() });

        let collector = job.clone();
        tokio::spawn(async move {
            while let Some(e) = rx.recv().await {
                collector.push(e);
            }
        });

        let st = self.clone();
        let j = job.clone();
        let ev = events.clone();
        // take a place in line now, so the position is right from the first status request
        let ticket = self.queue.enqueue(&job.id, false);
        if !ticket.is_ready() {
            let ahead = self.queue.position(&job.id).unwrap_or(1) - 1;
            events.status(match ahead {
                0 => "Queued — waiting for the current job to finish".to_string(),
                n => format!("Queued — {n} job(s) ahead of this one"),
            });
        }
        let work = tokio::spawn(async move {
            j.control.set_slot(ticket.wait().await);
            {
                let mut s = j.status.lock().unwrap();
                if *s == JobStatus::Queued {
                    *s = JobStatus::Running;
                }
            }
            j.mark_running();
            j.tick.send_modify(|_| {});
            st.engine.run_controlled(req, ev, j.control.clone()).await
        });
        *job.abort.lock().unwrap() = Some(work.abort_handle());

        // Whatever happens to the work — success, error, cancellation, even a panic — the job ends
        // with exactly one `done` or `error` event, so no client is ever left waiting.
        let j = job.clone();
        let st = self.clone();
        tokio::spawn(async move {
            let outcome = work.await;
            // the slot goes to the next job whatever happened
            j.control.release_slot();
            j.queue.remove(&j.id);
            match outcome {
                Ok(Ok(r)) => {
                    st.record_latency(r.depth.as_str(), r.stats.duration_ms);
                    events.emit(Event::Done { result: Box::new(r) })
                }
                Ok(Err(e)) => {
                    tracing::warn!("job {} failed: {e:#}", j.id);
                    events.emit(Event::Error { message: format!("{e:#}") });
                }
                Err(e) if e.is_cancelled() => {
                    events.emit(Event::Error { message: "cancelled".into() });
                }
                Err(e) => {
                    tracing::error!("job {} crashed: {e}", j.id);
                    events.emit(Event::Error { message: format!("internal error: the job crashed ({e})") });
                }
            }
        });
        Ok(job)
    }

    fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    /// Drop finished jobs older than an hour.
    pub fn prune(&self) {
        self.jobs.lock().unwrap().retain(|_, j| !(j.finished() && j.created.elapsed() > Duration::from_secs(3600)));
    }
}

// ----------------------------------------------------------------- errors

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut resp = (self.0, Json(json!({ "error": self.1 }))).into_response();
        if self.0 == StatusCode::SERVICE_UNAVAILABLE {
            resp.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
        }
        resp
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("{what} not found"))
}

fn validate(req: &ResearchRequest) -> Result<(), ApiError> {
    if req.query.trim().is_empty() {
        return Err(bad("`query` must not be empty"));
    }
    if req.query.chars().count() > 4000 {
        return Err(bad("`query` is too long (max 4000 characters)"));
    }
    if let Some(d) = &req.depth
        && !d.eq_ignore_ascii_case("auto")
        && Depth::parse(d).is_none()
    {
        return Err(bad("`depth` must be one of: auto, quick, standard, deep"));
    }
    if req.max_words.is_some_and(|n| !(20..=5000).contains(&n)) {
        return Err(bad("`max_words` must be between 20 and 5000"));
    }
    if req.report.is_none() && (req.sources.is_some() || req.web.is_some()) {
        return Err(bad("`sources` and `web` only apply to follow-ups: set `report` to the report id"));
    }
    if req.report.is_some()
        && let Err(e) = crate::followup::WebMode::parse(req.web.as_deref())
    {
        return Err(bad(format!("{e}")));
    }
    Ok(())
}

/// A follow-up's report must exist (and the sources it names, too) — fail fast with 404.
async fn check_report(st: &AppState, req: &ResearchRequest) -> Result<(), ApiError> {
    let Some(id) = req.report.as_deref() else { return Ok(()) };
    let r = st.engine.store.get(id).await.ok_or_else(|| not_found("report"))?;
    let (mentioned, _) = crate::followup::mentions(&req.query);
    for n in req.sources.iter().flatten().chain(mentioned.iter()) {
        if !r.sources.iter().any(|s| s.n == *n) {
            return Err(ApiError(StatusCode::NOT_FOUND, format!("report {id} has no source [{n}]")));
        }
    }
    Ok(())
}

fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    match headers.get("idempotency-key").map(|v| v.to_str()) {
        None => Ok(None),
        Some(Ok(k)) if !k.trim().is_empty() && k.len() <= 200 => Ok(Some(k.trim().to_string())),
        Some(_) => Err(bad("`Idempotency-Key` must be 1-200 visible characters")),
    }
}

// ---------------------------------------------------------------- handlers

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

fn asset(body: &'static str, ctype: &'static str) -> Response {
    ([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

async fn health(State(st): State<AppState>) -> Json<Value> {
    let info = st.engine.llm.info().await;
    let ok = info.reachable && info.available;
    let search = st.engine.tools.search.as_ref().map(|s| s.status()).unwrap_or_default();
    let backends: Vec<&str> = search.iter().map(|b| b.name).collect();
    Json(json!({
        "status": if ok { "ok" } else { "degraded" },
        "version": VERSION,
        "model": info,
        "fast_model": info.fast_model.clone(),
        "tools": st.engine.tools.names(),
        "search_backends": backends,
        "search_health": search,
        "search_mode": format!("{:?}", st.engine.cfg.search_mode).to_lowercase(),
        "jobs_running": st.count(JobStatus::Running),
        "jobs_queued": st.count(JobStatus::Queued),
        "jobs_paused": st.count(JobStatus::Paused),
        "max_concurrent": st.engine.cfg.max_concurrent,
        "max_queue": st.engine.cfg.max_queue,
        "watchdog": st.engine.cfg.watch_url.is_some(),
        "num_ctx": st.engine.cfg.num_ctx,
        "agent_mode": format!("{:?}", st.engine.cfg.agent_mode).to_lowercase(),
        "auth_required": st.auth.required(),
        "uptime_secs": st.started.elapsed().as_secs(),
        "num_thread": st.engine.cfg.num_thread,
        "fact_check": st.engine.cfg.fact_check,
    }))
}

pub fn manifest_json(st: &AppState) -> Value {
    json!({
        "name": "cookie-web",
        "version": VERSION,
        "kind": "agent",
        "description": "Web research agent: a small local LLM (Ollama) with web search, page reading, crawling and \
                        Wikipedia tools. Give it a request in natural language; it decides how deep to go and \
                        returns a Markdown answer with numbered, linked sources.",
        "model": st.engine.cfg.model,
        "capabilities": ["web_search", "news_search", "read_web_pages", "read_pdfs", "crawl_sites", "wikipedia", "cited_markdown_reports", "follow_up_questions", "source_content", "source_images"],
        "tools": st.engine.tools.names(),
        "input": {
            "query": "string (required) — the request; prefix with !quick / !standard / !deep to force depth",
            "depth": "auto | quick | standard | deep (default auto)",
            "model": "string (optional) — Ollama model override",
            "save": "bool (default true) — persist the Markdown report",
            "report": "string (optional) — follow up on this report id: answered from its sources, same citation numbers; @3 in the query = its source [3]",
            "sources": "[int] (optional, with report) — only these sources of the report",
            "web": "auto | never | always (with report; default auto = search only if the sources don't cover it)",
            "max_words": "int 20-5000 (optional) — upper limit on the answer's length; shorter is faster"
        },
        "output": "ResearchResult { id, query, depth, intent, triage, markdown, sources[], stats, model, created_at, report_path, warnings[], degraded }",
        "reliability": {
            "idempotency": "send an Idempotency-Key header to make retried POSTs reattach to the same job",
            "resume": "reconnect to /api/jobs/{id}/events with ?from=<last event id> (or Last-Event-ID)",
            "degraded": "if the model fails, the answer is a digest of the sources and degraded=true"
        },
        "endpoints": {
            "research": { "method": "POST", "path": "/api/research", "returns": "ResearchResult (JSON) or text/markdown" },
            "stream": { "method": "POST", "path": "/api/research/stream", "returns": "text/event-stream" },
            "submit_job": { "method": "POST", "path": "/api/jobs", "returns": "202 Job" },
            "job": { "method": "GET", "path": "/api/jobs/{id}" },
            "job_events": { "method": "GET", "path": "/api/jobs/{id}/events?from={n}", "returns": "text/event-stream (replays from event n, default the start)" },
            "cancel_job": { "method": "DELETE or POST", "path": "/api/jobs/{id} or /api/jobs/{id}/cancel" },
            "pause_job": { "method": "POST", "path": "/api/jobs/{id}/pause", "returns": "Job (status paused once it has stopped)" },
            "resume_job": { "method": "POST", "path": "/api/jobs/{id}/resume", "returns": "Job (goes to the front of the queue)" },
            "reports": { "method": "GET", "path": "/api/reports" },
            "ask": { "method": "POST", "path": "/api/reports/{id}/ask", "body": "{question, sources?, web?, depth?}", "returns": "ResearchResult (follow-up)" },
            "report_sources": { "method": "GET", "path": "/api/reports/{id}/sources" },
            "source_content": { "method": "GET", "path": "/api/reports/{id}/sources/{n}?format=json|markdown|text&focus=&max_chars=", "returns": "the source's full content and images" },
            "source_image": { "method": "GET", "path": "/api/reports/{id}/sources/{n}/images/{k}", "returns": "image bytes" },
            "report_content": { "method": "GET", "path": "/api/reports/{id}/content?sources=1,3&format=json|markdown|text&focus=", "returns": "all (cited/read) sources' content" },
            "report": { "method": "GET", "path": "/api/reports/{id}" },
            "report_markdown": { "method": "GET", "path": "/api/reports/{id}/markdown" },
            "health": { "method": "GET", "path": "/api/health" },
            "status": { "method": "GET", "path": "/api/status", "returns": "live: service, queue, active + recent jobs, model, search engines, latency, watchdog" },
            "status_stream": { "method": "GET", "path": "/api/status/stream?interval=2", "returns": "text/event-stream of `status` events (the /api/status document)" },
            "watch_status": { "method": "GET", "path": "/watch/status", "returns": "the watchdog's view; answers even when cookie-web is down (via nginx)" },
            "tools": { "method": "GET", "path": "/api/tools" }
        },
        "contract": contract(st),
        "auth": if st.auth.required() { "Authorization: Bearer <key> or X-API-Key header (or a signed-in session)" } else { "none" },
        "docs": "/docs/API.md in the repository"
    })
}

/// Everything live in one document: service, queue, active and recent jobs, model, watchdog.
async fn live_status(st: &AppState) -> Value {
    let (model, watch) = {
        let mut c = st.status_cache.lock().await;
        let fresh = |t: &Instant, secs: u64| t.elapsed() < Duration::from_secs(secs);
        if !c.model.as_ref().is_some_and(|(t, _)| fresh(t, 10)) {
            c.model = Some((Instant::now(), st.engine.llm.info().await));
        }
        if let Some(url) = st.engine.cfg.watch_url.as_deref()
            && !c.watch.as_ref().is_some_and(|(t, _)| fresh(t, 3))
        {
            let mut req = reqwest::Client::new().get(format!("{url}/watch/status")).timeout(Duration::from_secs(2));
            if let Some(k) = &st.engine.cfg.api_key {
                req = req.bearer_auth(k);
            }
            let v = match req.send().await {
                Ok(r) => r.json::<Value>().await.unwrap_or(Value::Null),
                Err(e) => json!({ "error": format!("watchdog not reachable: {e}") }),
            };
            c.watch = Some((Instant::now(), v));
        }
        (c.model.as_ref().map(|m| m.1.clone()).unwrap_or_default(), c.watch.as_ref().map(|w| w.1.clone()))
    };
    let jobs: Vec<Arc<Job>> = st.jobs.lock().unwrap().values().cloned().collect();
    let rank = |j: &Job| match j.status() {
        JobStatus::Running => (0, 0),
        JobStatus::Paused => (1, 0),
        JobStatus::Queued => (2, j.queue.position(&j.id).unwrap_or(usize::MAX)),
        _ => (3, 0),
    };
    let mut active: Vec<&Arc<Job>> = jobs.iter().filter(|j| !j.finished()).collect();
    active.sort_by_key(|j| rank(j));
    let mut recent: Vec<&Arc<Job>> = jobs.iter().filter(|j| j.finished()).collect();
    recent.sort_by_key(|j| std::cmp::Reverse(j.created));
    let ok = model.reachable && model.available;
    json!({
        "status": if ok { "ok" } else { "degraded" },
        "at": chrono::Utc::now(),
        "service": {
            "name": "cookie-web", "version": VERSION, "pid": std::process::id(),
            "started_at": st.started_at, "uptime_secs": st.started.elapsed().as_secs(),
        },
        "queue": {
            "max_concurrent": st.queue.capacity(), "max_queue": st.engine.cfg.max_queue,
            "slots_in_use": st.queue.running(), "waiting": st.queue.waiting(),
            "running": st.count(JobStatus::Running), "paused": st.count(JobStatus::Paused),
            "queued": st.count(JobStatus::Queued),
        },
        "jobs": active.iter().map(|j| j.snapshot(false)).collect::<Vec<_>>(),
        "recent_jobs": recent.iter().take(10).map(|j| j.snapshot(false)).collect::<Vec<_>>(),
        "model": {
            "main": model.model, "fast": model.fast_model, "fast_active": model.fast_active,
            "reachable": model.reachable, "available": model.available, "loaded": model.loaded,
            "memory_bytes": model.memory_bytes, "error": model.error,
        },
        "search": st.engine.tools.search.as_ref().map(|s| s.status()).unwrap_or_default(),
        "latency": st.latency(),
        "watchdog": watch,
    })
}

async fn status(State(st): State<AppState>) -> Json<Value> {
    Json(live_status(&st).await)
}

#[derive(Deserialize)]
struct EveryQ {
    interval: Option<u64>,
}

/// `GET /api/status/stream` — the same document as `/api/status`, pushed every `interval` seconds
/// (default 2) as `status` events.
async fn status_stream(State(st): State<AppState>, Query(q): Query<EveryQ>) -> Response {
    let every = Duration::from_secs(q.interval.unwrap_or(2).clamp(1, 60));
    let stream = async_stream::stream! {
        let mut n = 0u64;
        loop {
            n += 1;
            let v = live_status(&st).await;
            yield Ok::<_, Infallible>(SseEvent::default().event("status").id(n.to_string()).data(v.to_string()));
            tokio::time::sleep(every).await;
        }
    };
    let mut resp = Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response();
    resp.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
    resp
}

/// What a caller can rely on: limits, queueing, time bounds and observed latency.
fn contract(st: &AppState) -> Value {
    let cfg = &st.engine.cfg;
    json!({
        "one_job_at_a_time": cfg.max_concurrent == 1,
        "max_concurrent": cfg.max_concurrent,
        "max_queue": cfg.max_queue,
        "queue": "first come, first served; past max_queue a new job gets 503 with Retry-After. A resumed job goes to the front.",
        "job_timeout_secs": cfg.job_timeout.as_secs(),
        "time_bound": "every job ends with exactly one `done` or `error` event within job_timeout_secs of working time (paused time doesn't count)",
        "pause": "POST /api/jobs/{id}/pause stops the model at once and frees the slot for the next job; /resume carries on where it stopped",
        "cancel": "DELETE /api/jobs/{id} (or POST …/cancel) stops a queued, running or paused job at once",
        "latency": st.latency(),
        "latency_note": "median / 90th-percentile working time of recent jobs per depth on this machine; empty until jobs have run",
        "limits": { "query_chars": 4000, "max_words": [20, 5000] },
        "model_calls": "one call to the main model per answer (it writes); small steps use the fast model",
        "degraded": "if the model fails, the answer is a digest of the sources and degraded=true — never an empty result",
    })
}

async fn manifest(State(st): State<AppState>) -> Json<Value> {
    Json(manifest_json(&st))
}

async fn tools(State(st): State<AppState>) -> Json<Value> {
    Json(json!({ "tools": st.engine.tools.specs() }))
}

#[derive(Deserialize, Default)]
struct FormatQ {
    format: Option<String>,
}

fn wants_markdown(q: &FormatQ, headers: &HeaderMap) -> bool {
    q.format.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("markdown") || f.eq_ignore_ascii_case("md"))
        || headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.starts_with("text/markdown"))
}

fn markdown_response(md: String) -> Response {
    ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], md).into_response()
}

async fn research(
    State(st): State<AppState>,
    Query(fq): Query<FormatQ>,
    headers: HeaderMap,
    Json(req): Json<ResearchRequest>,
) -> Result<Response, ApiError> {
    validate(&req)?;
    check_report(&st, &req).await?;
    let job = st.spawn(req, idempotency_key(&headers)?)?;
    finished_response(job, &fq, &headers).await
}

/// Wait for a job and answer with its result (JSON, or Markdown if asked for).
async fn finished_response(job: Arc<Job>, fq: &FormatQ, headers: &HeaderMap) -> Result<Response, ApiError> {
    job.wait().await;
    if let Some(r) = job.result.lock().unwrap().clone() {
        if wants_markdown(fq, headers) {
            return Ok(markdown_response(r.markdown));
        }
        let mut resp = Json(r).into_response();
        resp.headers_mut().insert("x-cookie-job", HeaderValue::from_str(&job.id).unwrap());
        return Ok(resp);
    }
    let err = job.error.lock().unwrap().clone().unwrap_or_else(|| "job ended without a result".into());
    Err(ApiError(StatusCode::BAD_GATEWAY, err))
}

/// SSE view of a job, starting after event number `from` (0 = from the beginning).
fn sse_for(job: Arc<Job>, from: usize) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = async_stream::stream! {
        let mut rx = job.tick.subscribe();
        let mut idx = from.min(job.events.lock().unwrap().len());
        loop {
            let batch: Vec<Event> = {
                let ev = job.events.lock().unwrap();
                ev[idx.min(ev.len())..].to_vec()
            };
            for e in batch {
                idx += 1;
                let data = serde_json::to_string(&e).unwrap_or_else(|_| "{}".into());
                yield Ok(SseEvent::default().event(e.name()).id(idx.to_string()).data(data));
            }
            if job.finished() && idx >= job.events.lock().unwrap().len() {
                break;
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn research_stream(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ResearchRequest>,
) -> Result<Response, ApiError> {
    validate(&req)?;
    check_report(&st, &req).await?;
    let job = st.spawn(req, idempotency_key(&headers)?)?;
    let mut resp = sse_for(job.clone(), 0).into_response();
    resp.headers_mut().insert("x-cookie-job", HeaderValue::from_str(&job.id).unwrap());
    resp.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(resp)
}

async fn submit_job(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ResearchRequest>,
) -> Result<Response, ApiError> {
    validate(&req)?;
    check_report(&st, &req).await?;
    let job = st.spawn(req, idempotency_key(&headers)?)?;
    let mut body = job.snapshot(false);
    body["status_url"] = json!(format!("/api/jobs/{}", job.id));
    Ok((StatusCode::ACCEPTED, [(header::LOCATION, format!("/api/jobs/{}", job.id))], Json(body)).into_response())
}

async fn list_jobs(State(st): State<AppState>) -> Json<Value> {
    let mut jobs: Vec<(chrono::DateTime<chrono::Utc>, Value)> =
        st.jobs.lock().unwrap().values().map(|j| (j.created_at, j.snapshot(false))).collect();
    jobs.sort_by_key(|j| std::cmp::Reverse(j.0));
    Json(json!({ "jobs": jobs.into_iter().map(|(_, v)| v).collect::<Vec<_>>() }))
}

async fn get_job(State(st): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    st.get(&id).map(|j| Json(j.snapshot(true))).ok_or_else(|| not_found("job"))
}

#[derive(Deserialize)]
struct FromQ {
    from: Option<usize>,
}

async fn job_events(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<FromQ>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let job = st.get(&id).ok_or_else(|| not_found("job"))?;
    let last_id = headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok());
    let mut resp = sse_for(job, q.from.or(last_id).unwrap_or(0)).into_response();
    resp.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(resp)
}

async fn cancel_job(State(st): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let job = st.get(&id).ok_or_else(|| not_found("job"))?;
    if job.cancel() {
        tracing::info!("job {id} cancelled by the client");
    }
    Ok(Json(job.snapshot(false)))
}

async fn pause_job(State(st): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let job = st.get(&id).ok_or_else(|| not_found("job"))?;
    job.pause().map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    tracing::info!("job {id}: pause requested");
    Ok(Json(job.snapshot(false)))
}

async fn resume_job(State(st): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let job = st.get(&id).ok_or_else(|| not_found("job"))?;
    job.resume().map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    tracing::info!("job {id}: resume requested");
    Ok(Json(job.snapshot(false)))
}

#[derive(Deserialize)]
struct ListQ {
    limit: Option<usize>,
}

async fn list_reports(State(st): State<AppState>, Query(q): Query<ListQ>) -> Json<Value> {
    Json(json!({ "reports": st.engine.store.list(q.limit.unwrap_or(50).min(500)).await }))
}

async fn get_report(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(fq): Query<FormatQ>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let r = st.engine.store.get(&id).await.ok_or_else(|| not_found("report"))?;
    if wants_markdown(&fq, &headers) {
        return Ok(markdown_response(r.markdown));
    }
    Ok(Json(r).into_response())
}

async fn report_markdown(State(st): State<AppState>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let md = st.engine.store.markdown(&id).await.ok_or_else(|| not_found("report"))?;
    let mut resp = markdown_response(md);
    if let Ok(v) = HeaderValue::from_str(&format!("inline; filename=\"{id}.md\"")) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

async fn delete_report(State(st): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    if st.engine.store.delete(&id).await { Ok(StatusCode::NO_CONTENT) } else { Err(not_found("report")) }
}

// ---------------------------------------------------------- follow-ups & sources

#[derive(Deserialize)]
struct AskBody {
    question: String,
    #[serde(default)]
    sources: Option<Vec<usize>>,
    #[serde(default)]
    web: Option<String>,
    #[serde(default)]
    depth: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default = "yes")]
    save: bool,
    #[serde(default)]
    max_words: Option<u32>,
}

fn yes() -> bool {
    true
}

/// `POST /api/reports/{id}/ask` — a follow-up question on a report (blocking).
async fn ask(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(fq): Query<FormatQ>,
    headers: HeaderMap,
    Json(b): Json<AskBody>,
) -> Result<Response, ApiError> {
    let req = ResearchRequest {
        query: b.question,
        depth: b.depth,
        model: b.model,
        save: b.save,
        report: Some(id),
        sources: b.sources,
        web: b.web,
        max_words: b.max_words,
    };
    validate(&req)?;
    check_report(&st, &req).await?;
    let job = st.spawn(req, idempotency_key(&headers)?)?;
    finished_response(job, &fq, &headers).await
}

#[derive(Deserialize, Default)]
struct ContentQ {
    /// json (default), markdown or text
    format: Option<String>,
    /// only the passages relevant to this
    focus: Option<String>,
    max_chars: Option<usize>,
    /// for /content: which sources, e.g. "1,3"
    sources: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Fmt {
    Json,
    Markdown,
    Text,
}

fn content_format(q: &ContentQ, headers: &HeaderMap) -> Result<Fmt, ApiError> {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).unwrap_or("");
    match q.format.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("json") => Ok(Fmt::Json),
        Some("markdown" | "md") => Ok(Fmt::Markdown),
        Some("text" | "txt" | "plain") => Ok(Fmt::Text),
        Some(other) => Err(bad(format!("unknown format '{other}': use json, markdown or text"))),
        None if accept.starts_with("text/markdown") => Ok(Fmt::Markdown),
        None if accept.starts_with("text/plain") => Ok(Fmt::Text),
        None => Ok(Fmt::Json),
    }
}

/// Default size of a source's content in a response (a whole page, for nearly every page).
const SOURCE_CHARS: usize = 60_000;

fn page_body(page: &crate::tools::extract::Page, q: &ContentQ, per_source: usize) -> String {
    let max = q.max_chars.unwrap_or(per_source).clamp(500, 2_000_000);
    match q.focus.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => crate::tools::extract::focus_excerpt(&page.markdown, Some(f), max),
        None => crate::tools::extract::truncate_chars(&page.markdown, max),
    }
}

fn image_links(id: &str, n: usize, page: &crate::tools::extract::Page) -> Vec<Value> {
    page.images
        .iter()
        .enumerate()
        .map(|(i, img)| {
            json!({ "k": i + 1, "url": img.url, "alt": img.alt,
                    "proxy": format!("/api/reports/{id}/sources/{n}/images/{}", i + 1) })
        })
        .collect()
}

fn source_markdown(n: usize, page: &crate::tools::extract::Page, body: &str, source_url: &str) -> String {
    let mut out = format!("# {}\n\nSource [{n}]: <{source_url}>\n", page.title);
    if page.url != source_url {
        out.push_str(&format!("Read from: <{}>\n", page.url));
    }
    if let Some(p) = &page.published {
        out.push_str(&format!("Published: {p}\n"));
    }
    out.push_str(&format!("\n{}\n", body.trim()));
    out
}

fn source_text(n: usize, page: &crate::tools::extract::Page, body: &str, source_url: &str) -> String {
    let mut out = crate::report::md_to_text(&source_markdown(n, page, body, source_url));
    if !page.images.is_empty() {
        out.push_str("\n\nImages:\n");
        for img in &page.images {
            out.push_str(&format!(
                "- {}{}\n",
                img.url,
                if img.alt.is_empty() { String::new() } else { format!(" — {}", img.alt) }
            ));
        }
    }
    out
}

fn text_response(body: String, fmt: Fmt) -> Response {
    let ctype = if fmt == Fmt::Text { "text/plain; charset=utf-8" } else { "text/markdown; charset=utf-8" };
    ([(header::CONTENT_TYPE, ctype)], body).into_response()
}

/// Read failures: a missing source is 404, a page that can't be read 502.
fn read_error(e: anyhow::Error) -> ApiError {
    let m = format!("{e:#}");
    if m.contains("has no source") { ApiError(StatusCode::NOT_FOUND, m) } else { ApiError(StatusCode::BAD_GATEWAY, m) }
}

/// `GET /api/reports/{id}/sources` — the numbered sources and whether their full text is stored.
async fn report_sources(State(st): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let r = st.engine.store.get(&id).await.ok_or_else(|| not_found("report"))?;
    let pages = st.engine.store.pages(&id).await;
    let sources: Vec<Value> = r
        .sources
        .iter()
        .map(|s| {
            let mut v = serde_json::to_value(s).unwrap_or_default();
            v["stored"] = json!(pages.contains_key(&s.n));
            v["images"] = json!(pages.get(&s.n).map_or(0, |p| p.images.len()));
            v["content_url"] = json!(format!("/api/reports/{id}/sources/{}", s.n));
            v
        })
        .collect();
    Ok(Json(json!({ "report": id, "query": r.query, "sources": sources })))
}

/// `GET /api/reports/{id}/sources/{n}` — the full content of one source (read now if needed).
async fn source_content(
    State(st): State<AppState>,
    Path((id, n)): Path<(String, usize)>,
    Query(q): Query<ContentQ>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let fmt = content_format(&q, &headers)?;
    let r = st.engine.store.get(&id).await.ok_or_else(|| not_found("report"))?;
    let page = crate::followup::source_page(&st.engine, &r, n).await.map_err(read_error)?;
    let src_url = r.sources.iter().find(|s| s.n == n).map(|s| s.url.clone()).unwrap_or_default();
    let body = page_body(&page, &q, SOURCE_CHARS);
    Ok(match fmt {
        Fmt::Json => Json(json!({
            "report": id, "n": n, "title": page.title, "url": src_url, "read_from": page.url,
            "published": page.published, "byline": page.byline, "site_name": page.site_name,
            "words": page.markdown.split_whitespace().count(),
            "truncated": body.chars().count() < page.markdown.chars().count(),
            "content": body, "images": image_links(&id, n, &page),
        }))
        .into_response(),
        Fmt::Markdown => text_response(source_markdown(n, &page, &body, &src_url), fmt),
        Fmt::Text => text_response(source_text(n, &page, &body, &src_url), fmt),
    })
}

/// `GET /api/reports/{id}/sources/{n}/images/{k}` — one of the source's images, fetched for the caller.
async fn source_image(
    State(st): State<AppState>,
    Path((id, n, k)): Path<(String, usize, usize)>,
) -> Result<Response, ApiError> {
    let r = st.engine.store.get(&id).await.ok_or_else(|| not_found("report"))?;
    let page = crate::followup::source_page(&st.engine, &r, n).await.map_err(read_error)?;
    let img = k.checked_sub(1).and_then(|i| page.images.get(i)).ok_or_else(|| not_found("image"))?;
    let web =
        st.engine.tools.web.as_ref().ok_or_else(|| ApiError(StatusCode::NOT_IMPLEMENTED, "no web access".into()))?;
    let (_, ctype, bytes) =
        web.get_bytes(&img.url).await.map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    if !ctype.starts_with("image/") {
        return Err(ApiError(StatusCode::BAD_GATEWAY, format!("{} is not an image ({ctype})", img.url)));
    }
    Ok(([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "max-age=86400".into())], bytes).into_response())
}

/// `GET /api/reports/{id}/content` — the content of the report's sources in one document:
/// the cited and read ones by default (off-topic search results left out), or `?sources=1,3`.
async fn report_content(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ContentQ>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    use futures::StreamExt;
    let fmt = content_format(&q, &headers)?;
    let r = st.engine.store.get(&id).await.ok_or_else(|| not_found("report"))?;
    let wanted: Vec<usize> = match q.sources.as_deref() {
        Some(list) => {
            let ns: Vec<usize> = list.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            if let Some(n) = ns.iter().find(|n| !r.sources.iter().any(|s| s.n == **n)) {
                return Err(ApiError(StatusCode::NOT_FOUND, format!("report {id} has no source [{n}]")));
            }
            ns
        }
        None => r.sources.iter().filter(|s| s.cited || s.read).map(|s| s.n).take(10).collect(),
    };
    let per = (q.max_chars.unwrap_or(SOURCE_CHARS * 2) / wanted.len().max(1)).max(500);
    let engine = st.engine.clone();
    let report = &r;
    let pages: Vec<(usize, anyhow::Result<Arc<crate::tools::extract::Page>>)> = futures::stream::iter(wanted)
        .map(|n| {
            let engine = engine.clone();
            async move { (n, crate::followup::source_page(&engine, report, n).await) }
        })
        .buffered(3)
        .collect()
        .await;
    let sub = ContentQ { max_chars: Some(per), ..q };
    let url_of = |n: usize| r.sources.iter().find(|s| s.n == n).map(|s| s.url.clone()).unwrap_or_default();
    match fmt {
        Fmt::Json => {
            let items: Vec<Value> = pages
                .iter()
                .map(|(n, p)| match p {
                    Ok(p) => json!({ "n": n, "title": p.title, "url": url_of(*n), "read_from": p.url,
                                     "content": page_body(p, &sub, per), "images": image_links(&id, *n, p) }),
                    Err(e) => json!({ "n": n, "url": url_of(*n), "error": format!("{e:#}") }),
                })
                .collect();
            Ok(Json(json!({ "report": id, "query": r.query, "sources": items })).into_response())
        }
        _ => {
            let mut out = format!("# Sources for: {}\n\n", r.query);
            for (n, p) in &pages {
                match p {
                    Ok(p) => {
                        let body = page_body(p, &sub, per);
                        out.push_str(&if fmt == Fmt::Text {
                            source_text(*n, p, &body, &url_of(*n))
                        } else {
                            source_markdown(*n, p, &body, &url_of(*n))
                        });
                    }
                    Err(e) => out.push_str(&format!("# [{n}] {}\n\n(could not be read: {e:#})\n", url_of(*n))),
                }
                out.push_str("\n\n");
            }
            Ok(text_response(if fmt == Fmt::Text { crate::report::md_to_text(&out) } else { out }, fmt))
        }
    }
}

// -------------------------------------------------------------------- auth

#[derive(Deserialize)]
struct KeyQ {
    api_key: Option<String>,
}

/// Paths anyone may load: the sign-in page and the assets it (and the app shell) needs, the
/// agent card, and the health check (which answers with less detail when not signed in).
fn public_path(path: &str) -> bool {
    matches!(path, "/login" | "/logout" | "/style.css" | "/favicon.svg" | "/app.js" | "/api/manifest" | "/api/health")
        || path.starts_with("/vendor/")
}

/// Is this request signed in (session cookie) or carrying the API key?
fn authorized(st: &AppState, req: &Request, query_key: Option<&str>) -> bool {
    let h = req.headers();
    let bearer = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let xkey = h.get("x-api-key").and_then(|v| v.to_str().ok());
    if bearer.or(xkey).or(query_key).is_some_and(|k| st.auth.check_key(k)) {
        return true;
    }
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(Auth::session_from)
        .any(|t| st.auth.valid_session(t))
}

async fn auth(State(st): State<AppState>, Query(q): Query<KeyQ>, req: Request, next: Next) -> Response {
    if !st.auth.required() {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    let ok = authorized(&st, &req, q.api_key.as_deref());
    if ok {
        return next.run(req).await;
    }
    if path == "/api/health" {
        // enough for a load balancer or supervisor to see it's up, nothing more
        return Json(json!({ "status": "ok", "version": VERSION, "auth_required": true })).into_response();
    }
    if public_path(&path) {
        return next.run(req).await;
    }
    if path.starts_with("/api/") {
        return ApiError(StatusCode::UNAUTHORIZED, "missing or invalid API key (or sign in)".into()).into_response();
    }
    let next_url = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let enc: String = url::form_urlencoded::byte_serialize(next_url.as_bytes()).collect();
    (StatusCode::SEE_OTHER, [(header::LOCATION, format!("/login?next={enc}"))]).into_response()
}

fn client_of(req_headers: &HeaderMap, peer: Option<std::net::SocketAddr>) -> String {
    // Behind the local reverse proxy (nginx), the client is in X-Real-IP / X-Forwarded-For. Those
    // headers are only trusted from this machine: anyone else could make them up to dodge the limit.
    let via_local_proxy = peer.is_none_or(|p| p.ip().is_loopback());
    let forwarded = || {
        let h = |n: &str| req_headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
        h("x-real-ip")
            .or_else(|| h("x-forwarded-for").and_then(|v| v.rsplit(',').next().map(str::to_string)))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    via_local_proxy
        .then(forwarded)
        .flatten()
        .or_else(|| peer.map(|p| p.ip().to_string()))
        .unwrap_or_else(|| "unknown".into())
}

fn session_cookie(value: &str, max_age: u64, secure: bool) -> String {
    format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        crate::auth::COOKIE,
        if secure { "; Secure" } else { "" }
    )
}

fn is_https(headers: &HeaderMap) -> bool {
    headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).is_some_and(|p| p.eq_ignore_ascii_case("https"))
}

async fn login_page(State(st): State<AppState>) -> Response {
    if !st.auth.required() {
        return (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
    }
    Html(include_str!("../static/login.html")).into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
    #[serde(default)]
    next: Option<String>,
}

async fn login(State(st): State<AppState>, req: Request) -> Response {
    let peer = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0);
    let headers = req.headers().clone();
    let client = client_of(&headers, peer);
    let wants_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/json"));
    let body = match axum::body::to_bytes(req.into_body(), 16 * 1024).await {
        Ok(b) => b,
        Err(_) => return bad("request too large").into_response(),
    };
    let form: Option<LoginForm> =
        if wants_json { serde_json::from_slice(&body).ok() } else { serde_urlencoded_form(&body) };
    let Some(form) = form else { return bad("send `password`").into_response() };
    // only same-site paths: never redirect to another site
    let next = form.next.filter(|n| n.starts_with('/') && !n.starts_with("//")).unwrap_or_else(|| "/".into());
    if !st.auth.required() {
        return (StatusCode::SEE_OTHER, [(header::LOCATION, next)]).into_response();
    }
    if let Some(wait) = st.auth.locked_out(&client) {
        tracing::warn!("login refused for {client}: too many wrong passwords");
        if wants_json {
            return ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                format!("too many wrong passwords; try again in {} minutes", wait.as_secs().div_ceil(60)),
            )
            .into_response();
        }
        return (StatusCode::SEE_OTHER, [(header::LOCATION, "/login?error=locked".to_string())]).into_response();
    }
    if !st.auth.check_password(&form.password) {
        st.auth.record_failure(&client);
        tracing::warn!("wrong password from {client}");
        if wants_json {
            return ApiError(StatusCode::UNAUTHORIZED, "wrong password".into()).into_response();
        }
        let enc: String = url::form_urlencoded::byte_serialize(next.as_bytes()).collect();
        return (StatusCode::SEE_OTHER, [(header::LOCATION, format!("/login?error=1&next={enc}"))]).into_response();
    }
    st.auth.clear_failures(&client);
    let cookie = session_cookie(&st.auth.issue(), crate::auth::SESSION_DAYS * 86_400, is_https(&headers));
    if wants_json {
        return ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response();
    }
    (StatusCode::SEE_OTHER, [(header::SET_COOKIE, cookie), (header::LOCATION, next)]).into_response()
}

/// `password=…&next=…` (browsers' form encoding).
fn serde_urlencoded_form(body: &[u8]) -> Option<LoginForm> {
    let mut password = None;
    let mut next = None;
    for (k, v) in url::form_urlencoded::parse(body) {
        match k.as_ref() {
            "password" => password = Some(v.into_owned()),
            "next" => next = Some(v.into_owned()),
            _ => {}
        }
    }
    Some(LoginForm { password: password?, next })
}

async fn logout(headers: HeaderMap) -> Response {
    let cookie = session_cookie("", 0, is_https(&headers));
    (StatusCode::SEE_OTHER, [(header::SET_COOKIE, cookie), (header::LOCATION, "/login".to_string())]).into_response()
}

// ------------------------------------------------------------------ router

pub fn router(st: AppState) -> Router {
    let cors = {
        let origins = &st.engine.cfg.cors_origins;
        let base = CorsLayer::new().allow_methods(Any).allow_headers(Any).expose_headers(Any);
        if origins.iter().any(|o| o == "*") {
            base.allow_origin(Any)
        } else {
            base.allow_origin(origins.iter().filter_map(|o| o.parse::<HeaderValue>().ok()).collect::<Vec<_>>())
        }
    };
    Router::new()
        .route("/", get(index))
        .route("/login", get(login_page).post(login))
        .route("/logout", get(logout).post(logout))
        .route("/app.js", get(|| async { asset(include_str!("../static/app.js"), "text/javascript; charset=utf-8") }))
        .route("/style.css", get(|| async { asset(include_str!("../static/style.css"), "text/css; charset=utf-8") }))
        .route("/favicon.svg", get(|| async { asset(include_str!("../static/favicon.svg"), "image/svg+xml") }))
        .route(
            "/vendor/marked.min.js",
            get(|| async { asset(include_str!("../static/vendor/marked.min.js"), "text/javascript; charset=utf-8") }),
        )
        .route(
            "/vendor/purify.min.js",
            get(|| async { asset(include_str!("../static/vendor/purify.min.js"), "text/javascript; charset=utf-8") }),
        )
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/status/stream", get(status_stream))
        .route("/api/manifest", get(manifest))
        .route("/api/tools", get(tools))
        .route("/api/research", post(research))
        .route("/api/research/stream", post(research_stream))
        .route("/api/jobs", post(submit_job).get(list_jobs))
        .route("/api/jobs/{id}", get(get_job).delete(cancel_job))
        .route("/api/jobs/{id}/events", get(job_events))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        .route("/api/jobs/{id}/pause", post(pause_job))
        .route("/api/jobs/{id}/resume", post(resume_job))
        .route("/api/reports", get(list_reports))
        .route("/api/reports/{id}", get(get_report).delete(delete_report))
        .route("/api/reports/{id}/markdown", get(report_markdown))
        .route("/api/reports/{id}/ask", post(ask))
        .route("/api/reports/{id}/content", get(report_content))
        .route("/api/reports/{id}/sources", get(report_sources))
        .route("/api/reports/{id}/sources/{n}", get(source_content))
        .route("/api/reports/{id}/sources/{n}/images/{k}", get(source_image))
        .fallback(|| async { ApiError(StatusCode::NOT_FOUND, "no such endpoint".into()) })
        .layer(middleware::from_fn_with_state(st.clone(), auth))
        .layer(cors)
        .with_state(st)
}

pub async fn serve(engine: Arc<Engine>) -> anyhow::Result<()> {
    let cfg = engine.cfg.clone();
    let st = AppState::new(engine);
    let pruner = st.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(600)).await;
            pruner.prune();
        }
    });
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on {addr}: {e} (is the port in use? set COOKIE_PORT)"))?;
    let all = matches!(cfg.host.as_str(), "0.0.0.0" | "::" | "[::]");
    tracing::info!(
        "cookie-web {VERSION} listening on http://{addr}{}  (model: {}, ollama: {})",
        if all {
            " — all interfaces, reachable at http://<this machine's IP>:".to_string() + &cfg.port.to_string()
        } else {
            String::new()
        },
        cfg.model,
        cfg.ollama_host
    );
    let loopback = matches!(cfg.host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]");
    if !loopback && cfg.api_key.is_none() && cfg.password.is_none() {
        tracing::warn!(
            "no COOKIE_API_KEY is set and cookie-web is reachable from the network: anyone who can reach port {} \
             can use it. Set COOKIE_API_KEY and/or COOKIE_PASSWORD (or COOKIE_HOST=127.0.0.1 to keep it local).",
            cfg.port
        );
    }
    // systemd: ready to serve, and keep its watchdog fed while the server answers
    let bound = listener.local_addr().map(|a| a.port()).unwrap_or(cfg.port);
    crate::watch::sd_notify("READY=1");
    crate::watch::start_systemd_watchdog(bound);
    if let Some(url) = cfg.watch_url.clone() {
        let s = st.clone();
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(cfg.port);
        let name = crate::watch::Beat::own_name();
        tracing::info!("registering with the watchdog at {url}");
        crate::watch::start_heartbeat(url, move || crate::watch::Beat {
            pid: std::process::id(),
            name: name.clone(),
            version: VERSION.into(),
            port,
            started_at: s.started_at,
            jobs_running: s.count(JobStatus::Running),
            jobs_queued: s.count(JobStatus::Queued),
            jobs_paused: s.count(JobStatus::Paused),
        });
    }
    // connect info: the client address, for rate-limiting wrong passwords
    axum::serve(listener, router(st).into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal(cfg.watch_url.clone()))
        .await?;
    Ok(())
}

/// Ctrl-C or SIGTERM (systemd, Docker). Open SSE streams get 10 s to finish, then the process exits.
async fn shutdown_signal(watch_url: Option<String>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutting down");
    crate::watch::sd_notify("STOPPING=1");
    if let Some(url) = watch_url {
        crate::watch::say_bye(&url).await;
    }
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        std::process::exit(0);
    });
}
