//! Page fetching against a local fake web server: retries, archive fallback, encodings, budgets.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use cookie_web::{
    config::Config,
    events::Events,
    tools::{Tool, ToolCtx, fetch::FetchPage, net::Web},
};
use serde_json::json;

const ARTICLE: &str = "<html><head><title>Fake Article</title></head><body><article><h1>Fake Article</h1>\
    <p>The quick brown fox jumps over the lazy dog, again and again, to make this paragraph long enough \
    for the readability extractor to treat it as the main content of the page.</p>\
    <p>A second paragraph with more words so that there is plenty of text to extract here.</p></article></body></html>";

#[derive(Clone, Default)]
struct Site {
    hits: Arc<AtomicUsize>,
    agents: Arc<Mutex<Vec<String>>>,
    base: Arc<Mutex<String>>,
}

async fn flaky(State(s): State<Site>) -> Response {
    if s.hits.fetch_add(1, Ordering::SeqCst) == 0 {
        return (StatusCode::SERVICE_UNAVAILABLE, "try later").into_response();
    }
    ([("content-type", "text/html")], ARTICLE).into_response()
}

async fn picky(State(s): State<Site>, headers: HeaderMap) -> Response {
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    s.agents.lock().unwrap().push(ua.clone());
    if ua.contains("cookie-web") {
        return (StatusCode::FORBIDDEN, "no bots").into_response();
    }
    ([("content-type", "text/html")], ARTICLE).into_response()
}

async fn latin1() -> Response {
    let mut body = b"<html><head><title>Menu</title></head><body><article><p>".to_vec();
    body.extend_from_slice(b"Caf\xe9 cr\xe8me br\xfbl\xe9e. ".repeat(20).as_slice());
    body.extend_from_slice(b"</p></article></body></html>");
    ([("content-type", "text/html; charset=windows-1252")], body).into_response()
}

async fn gone() -> Response {
    (StatusCode::NOT_FOUND, "gone").into_response()
}

async fn wayback(State(s): State<Site>) -> Response {
    let base = s.base.lock().unwrap().clone();
    axum::Json(json!({ "archived_snapshots": { "closest": {
        "available": true, "status": "200", "timestamp": "20240102030405",
        "url": format!("{base}/web/20240102030405/{base}/gone")
    }}}))
    .into_response()
}

async fn snapshot(Path(_rest): Path<String>) -> Response {
    ([("content-type", "text/html")], ARTICLE).into_response()
}

async fn serve() -> (String, Site) {
    let site = Site::default();
    let app = Router::new()
        .route("/flaky", get(flaky))
        .route("/picky", get(picky))
        .route("/latin1", get(latin1))
        .route("/gone", get(gone))
        .route("/wayback/available", get(wayback))
        .route("/web/{*rest}", get(snapshot))
        .with_state(site.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    *site.base.lock().unwrap() = base.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, site)
}

fn web(base: &str, archive: bool) -> Web {
    let mut cfg = Config::for_tests("reports".into());
    cfg.allow_private_urls = true;
    cfg.archive_fallback = archive;
    cfg.user_agent = "Mozilla/5.0 cookie-web/0.1".into();
    let mut w = Web::new(Arc::new(cfg)).unwrap();
    w.archive_base = base.to_string();
    w.archive_web = base.to_string();
    w
}

#[tokio::test]
async fn transient_errors_are_retried() {
    let (base, site) = serve().await;
    let p = web(&base, false).page(&format!("{base}/flaky")).await.unwrap();
    assert!(p.markdown.contains("quick brown fox"));
    assert_eq!(site.hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn forbidden_is_retried_with_browser_headers() {
    let (base, site) = serve().await;
    let p = web(&base, false).page(&format!("{base}/picky")).await.unwrap();
    assert!(p.markdown.contains("quick brown fox"));
    let agents = site.agents.lock().unwrap().clone();
    assert_eq!(agents.len(), 2);
    assert!(!agents[1].contains("cookie-web"));
}

#[tokio::test]
async fn legacy_encodings_are_decoded() {
    let (base, _) = serve().await;
    let p = web(&base, false).page(&format!("{base}/latin1")).await.unwrap();
    assert!(p.markdown.contains("Café crème brûlée"), "{}", p.markdown);
}

#[tokio::test]
async fn dead_links_fall_back_to_the_wayback_machine() {
    let (base, _) = serve().await;
    let url = format!("{base}/gone");
    assert!(web(&base, false).page(&url).await.is_err());
    let p = web(&base, true).page(&url).await.unwrap();
    assert!(p.markdown.contains("quick brown fox"));
    assert!(p.title.contains("archived 2024-01-02"), "{}", p.title);
    assert!(p.url.contains("/web/20240102030405/"), "{}", p.url);
}

#[tokio::test]
async fn failed_reads_refund_the_page_budget() {
    let (base, _) = serve().await;
    let tool = FetchPage { web: Arc::new(web(&base, false)) };
    let ctx = ToolCtx::new(Events::none(), 5, 4000, 1);
    assert!(tool.call(json!({ "url": format!("{base}/gone") }), &ctx).await.is_err());
    assert_eq!(ctx.fetches_left(), 1, "budget refunded");
    assert!(tool.call(json!({ "url": format!("{base}/flaky") }), &ctx).await.is_ok());
    assert_eq!(ctx.fetches_left(), 0);
    // re-reading a page already read is free
    let again = tool.call(json!({ "url": format!("{base}/flaky") }), &ctx).await.unwrap();
    assert!(again.text.contains("quick brown fox"));
}
