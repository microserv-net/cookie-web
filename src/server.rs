//! HTTP API + embedded GUI. Every request runs as a *job*; the sync, SSE and
//! async (poll) endpoints are three views of the same job.

use std::{
    collections::HashMap,
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
use tokio::sync::{Semaphore, mpsc, watch};
use tower_http::cors::{Any, CorsLayer};

use crate::{
    depth::Depth,
    engine::{Engine, ResearchRequest, ResearchResult},
    events::{Event, Events},
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
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
}

impl Job {
    fn new(query: &str, key: Option<String>) -> Arc<Self> {
        let (tick, _) = watch::channel(0);
        Arc::new(Self {
            id: format!("job_{}", uuid::Uuid::new_v4().simple()),
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
        })
    }

    pub fn status(&self) -> JobStatus {
        *self.status.lock().unwrap()
    }

    fn finished(&self) -> bool {
        matches!(self.status(), JobStatus::Done | JobStatus::Error | JobStatus::Cancelled)
    }

    /// Stop the job if it is still queued or running. Returns false if it had already finished.
    pub fn cancel(&self) -> bool {
        {
            let mut st = self.status.lock().unwrap();
            if matches!(*st, JobStatus::Done | JobStatus::Error | JobStatus::Cancelled) {
                return false;
            }
            *st = JobStatus::Cancelled;
        }
        *self.error.lock().unwrap() = Some("cancelled".into());
        if let Some(h) = self.abort.lock().unwrap().as_ref() {
            h.abort();
        }
        self.tick.send_modify(|_| {});
        true
    }

    fn push(&self, e: Event) {
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
            "created_at": self.created_at,
            "events": self.events.lock().unwrap().len(),
            "events_url": format!("/api/jobs/{}/events", self.id),
        });
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
    pub engine: Arc<Engine>,
    pub jobs: Arc<Mutex<HashMap<String, Arc<Job>>>>,
    pub slots: Arc<Semaphore>,
}

impl AppState {
    pub fn new(engine: Arc<Engine>) -> Self {
        let slots = Arc::new(Semaphore::new(engine.cfg.max_concurrent));
        Self { engine, jobs: Arc::new(Mutex::new(HashMap::new())), slots }
    }

    /// Start a research job in the background. With an idempotency `key`, a request that repeats
    /// the key of a job still queued, running or done returns that job instead of starting another.
    pub fn spawn(&self, req: ResearchRequest, key: Option<String>) -> Arc<Job> {
        if let Some(k) = key.as_deref() {
            let existing = self
                .jobs
                .lock()
                .unwrap()
                .values()
                .find(|j| {
                    j.key.as_deref() == Some(k)
                        && matches!(j.status(), JobStatus::Queued | JobStatus::Running | JobStatus::Done)
                })
                .cloned();
            if let Some(j) = existing {
                return j;
            }
        }
        let job = Job::new(req.query.trim(), key);
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
        let work = tokio::spawn(async move {
            let permit = match st.slots.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    let ahead = st.queued_before(&j);
                    ev.status(match ahead {
                        0 => "Queued — waiting for the current job to finish".to_string(),
                        n => format!("Queued — {n} job(s) ahead of this one"),
                    });
                    st.slots.clone().acquire_owned().await.map_err(|_| anyhow::anyhow!("server is shutting down"))?
                }
            };
            {
                let mut s = j.status.lock().unwrap();
                if *s == JobStatus::Queued {
                    *s = JobStatus::Running;
                }
            }
            j.tick.send_modify(|_| {});
            let res = st.engine.run(req, ev).await;
            drop(permit);
            res
        });
        *job.abort.lock().unwrap() = Some(work.abort_handle());

        // Whatever happens to the work — success, error, cancellation, even a panic — the job ends
        // with exactly one `done` or `error` event, so no client is ever left waiting.
        let j = job.clone();
        tokio::spawn(async move {
            match work.await {
                Ok(Ok(r)) => events.emit(Event::Done { result: Box::new(r) }),
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
        job
    }

    /// Jobs queued before `job` (for the "waiting" message).
    fn queued_before(&self, job: &Job) -> usize {
        self.jobs
            .lock()
            .unwrap()
            .values()
            .filter(|j| j.id != job.id && j.created < job.created && j.status() == JobStatus::Queued)
            .count()
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
        (self.0, Json(json!({ "error": self.1 }))).into_response()
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
        "fast_model": st.engine.cfg.fast_model(),
        "tools": st.engine.tools.names(),
        "search_backends": backends,
        "search_health": search,
        "search_mode": format!("{:?}", st.engine.cfg.search_mode).to_lowercase(),
        "jobs_running": st.jobs.lock().unwrap().values().filter(|j| j.status() == JobStatus::Running).count(),
        "jobs_queued": st.jobs.lock().unwrap().values().filter(|j| j.status() == JobStatus::Queued).count(),
        "max_concurrent": st.engine.cfg.max_concurrent,
        "num_ctx": st.engine.cfg.num_ctx,
        "agent_mode": format!("{:?}", st.engine.cfg.agent_mode).to_lowercase(),
        "auth_required": st.engine.cfg.api_key.is_some(),
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
            "web": "auto | never | always (with report; default auto = search only if the sources don't cover it)"
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
            "cancel_job": { "method": "DELETE", "path": "/api/jobs/{id}" },
            "reports": { "method": "GET", "path": "/api/reports" },
            "ask": { "method": "POST", "path": "/api/reports/{id}/ask", "body": "{question, sources?, web?, depth?}", "returns": "ResearchResult (follow-up)" },
            "report_sources": { "method": "GET", "path": "/api/reports/{id}/sources" },
            "source_content": { "method": "GET", "path": "/api/reports/{id}/sources/{n}?format=json|markdown|text&focus=&max_chars=", "returns": "the source's full content and images" },
            "source_image": { "method": "GET", "path": "/api/reports/{id}/sources/{n}/images/{k}", "returns": "image bytes" },
            "report_content": { "method": "GET", "path": "/api/reports/{id}/content?sources=1,3&format=json|markdown|text&focus=", "returns": "all (cited/read) sources' content" },
            "report": { "method": "GET", "path": "/api/reports/{id}" },
            "report_markdown": { "method": "GET", "path": "/api/reports/{id}/markdown" },
            "health": { "method": "GET", "path": "/api/health" },
            "tools": { "method": "GET", "path": "/api/tools" }
        },
        "auth": if st.engine.cfg.api_key.is_some() { "Authorization: Bearer <key> or X-API-Key header" } else { "none" },
        "docs": "/docs/API.md in the repository"
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
    let job = st.spawn(req, idempotency_key(&headers)?);
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
    let job = st.spawn(req, idempotency_key(&headers)?);
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
    let job = st.spawn(req, idempotency_key(&headers)?);
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
    };
    validate(&req)?;
    check_report(&st, &req).await?;
    let job = st.spawn(req, idempotency_key(&headers)?);
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

async fn auth(State(st): State<AppState>, Query(q): Query<KeyQ>, req: Request, next: Next) -> Response {
    let Some(expected) = st.engine.cfg.api_key.as_deref() else { return next.run(req).await };
    let path = req.uri().path();
    if !path.starts_with("/api/") || path == "/api/health" || path == "/api/manifest" {
        return next.run(req).await;
    }
    let h = req.headers();
    let bearer = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let xkey = h.get("x-api-key").and_then(|v| v.to_str().ok());
    let given = bearer.or(xkey).or(q.api_key.as_deref());
    if given.is_some_and(|g| constant_time_eq(g.as_bytes(), expected.as_bytes())) {
        next.run(req).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "missing or invalid API key".into()).into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
        .route("/api/manifest", get(manifest))
        .route("/api/tools", get(tools))
        .route("/api/research", post(research))
        .route("/api/research/stream", post(research_stream))
        .route("/api/jobs", post(submit_job).get(list_jobs))
        .route("/api/jobs/{id}", get(get_job).delete(cancel_job))
        .route("/api/jobs/{id}/events", get(job_events))
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
    if !loopback && cfg.api_key.is_none() {
        tracing::warn!(
            "no COOKIE_API_KEY is set and cookie-web is reachable from the network: anyone who can reach port {} \
             can use it. Set COOKIE_API_KEY (or COOKIE_HOST=127.0.0.1 to keep it local).",
            cfg.port
        );
    }
    axum::serve(listener, router(st)).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

/// Ctrl-C or SIGTERM (systemd, Docker). Open SSE streams get 10 s to finish, then the process exits.
async fn shutdown_signal() {
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
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        std::process::exit(0);
    });
}
