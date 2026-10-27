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
}

impl Job {
    fn new(query: &str) -> Arc<Self> {
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
        })
    }

    pub fn status(&self) -> JobStatus {
        *self.status.lock().unwrap()
    }

    fn finished(&self) -> bool {
        matches!(self.status(), JobStatus::Done | JobStatus::Error)
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
            *self.status.lock().unwrap() = s;
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

    /// Start a research job in the background.
    pub fn spawn(&self, req: ResearchRequest) -> Arc<Job> {
        let job = Job::new(req.query.trim());
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
        tokio::spawn(async move {
            let Ok(_permit) = st.slots.clone().acquire_owned().await else { return };
            *j.status.lock().unwrap() = JobStatus::Running;
            j.tick.send_modify(|_| {});
            let res = st.engine.run(req, events.clone()).await;
            match res {
                Ok(r) => events.emit(Event::Done { result: Box::new(r) }),
                Err(e) => {
                    tracing::warn!("job {} failed: {e:#}", j.id);
                    events.emit(Event::Error { message: format!("{e:#}") });
                }
            }
        });
        job
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
    Ok(())
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
    Json(json!({
        "status": if ok { "ok" } else { "degraded" },
        "version": VERSION,
        "model": info,
        "fast_model": st.engine.cfg.fast_model(),
        "tools": st.engine.tools.names(),
        "search_backends": crate::tools::search::SearchEngine::from_config(&st.engine.cfg).names(),
        "search_mode": format!("{:?}", st.engine.cfg.search_mode).to_lowercase(),
        "jobs_running": st.jobs.lock().unwrap().values().filter(|j| j.status() == JobStatus::Running).count(),
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
        "capabilities": ["web_search", "news_search", "read_web_pages", "read_pdfs", "crawl_sites", "wikipedia", "cited_markdown_reports"],
        "tools": st.engine.tools.names(),
        "input": {
            "query": "string (required) — the request; prefix with !quick / !standard / !deep to force depth",
            "depth": "auto | quick | standard | deep (default auto)",
            "model": "string (optional) — Ollama model override",
            "save": "bool (default true) — persist the Markdown report"
        },
        "output": "ResearchResult { id, query, depth, intent, triage, markdown, sources[], stats, model, created_at, report_path }",
        "endpoints": {
            "research": { "method": "POST", "path": "/api/research", "returns": "ResearchResult (JSON) or text/markdown" },
            "stream": { "method": "POST", "path": "/api/research/stream", "returns": "text/event-stream" },
            "submit_job": { "method": "POST", "path": "/api/jobs", "returns": "202 Job" },
            "job": { "method": "GET", "path": "/api/jobs/{id}" },
            "job_events": { "method": "GET", "path": "/api/jobs/{id}/events", "returns": "text/event-stream (replays from start)" },
            "reports": { "method": "GET", "path": "/api/reports" },
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
    let job = st.spawn(req);
    job.wait().await;
    if let Some(r) = job.result.lock().unwrap().clone() {
        if wants_markdown(&fq, &headers) {
            return Ok(markdown_response(r.markdown));
        }
        let mut resp = Json(r).into_response();
        resp.headers_mut().insert("x-cookie-job", HeaderValue::from_str(&job.id).unwrap());
        return Ok(resp);
    }
    let err = job.error.lock().unwrap().clone().unwrap_or_else(|| "job ended without a result".into());
    Err(ApiError(StatusCode::BAD_GATEWAY, err))
}

fn sse_for(job: Arc<Job>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = async_stream::stream! {
        let mut rx = job.tick.subscribe();
        let mut idx = 0usize;
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

async fn research_stream(State(st): State<AppState>, Json(req): Json<ResearchRequest>) -> Result<Response, ApiError> {
    validate(&req)?;
    let job = st.spawn(req);
    let mut resp = sse_for(job.clone()).into_response();
    resp.headers_mut().insert("x-cookie-job", HeaderValue::from_str(&job.id).unwrap());
    resp.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(resp)
}

async fn submit_job(State(st): State<AppState>, Json(req): Json<ResearchRequest>) -> Result<Response, ApiError> {
    validate(&req)?;
    let job = st.spawn(req);
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

async fn job_events(State(st): State<AppState>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let job = st.get(&id).ok_or_else(|| not_found("job"))?;
    Ok(sse_for(job).into_response())
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
        .route("/api/health", get(health))
        .route("/api/manifest", get(manifest))
        .route("/api/tools", get(tools))
        .route("/api/research", post(research))
        .route("/api/research/stream", post(research_stream))
        .route("/api/jobs", post(submit_job).get(list_jobs))
        .route("/api/jobs/{id}", get(get_job))
        .route("/api/jobs/{id}/events", get(job_events))
        .route("/api/reports", get(list_reports))
        .route("/api/reports/{id}", get(get_report).delete(delete_report))
        .route("/api/reports/{id}/markdown", get(report_markdown))
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
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(
        "cookie-web {VERSION} listening on http://{addr}  (model: {}, ollama: {})",
        cfg.model,
        cfg.ollama_host
    );
    axum::serve(listener, router(st))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
