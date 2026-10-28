//! Follow-up questions and source content, end to end through the HTTP API: a real page reader
//! against a local fake website, a scripted model, and offline search pointing at that website.
mod common;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use cookie_web::{
    config::Config,
    server,
    sources::SourceKind,
    tools::{Registry, Tool, ToolCtx, ToolOutput, fetch::FetchPage, net::Web},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn article(title: &str, body: &str) -> String {
    format!(
        "<html><head><title>{title}</title><meta property=\"og:image\" content=\"/img/{title}.png\"></head><body>\
         <article><h1>{title}</h1><p>{body}</p><p>{body}</p><img src=\"/img/inline.png\" alt=\"A diagram of {title}\">\
         <p>Filler sentence that makes the article long enough for the readability extractor to keep it.</p></article></body></html>"
    )
}

#[derive(Clone, Default)]
struct Site {
    hits: Arc<Mutex<Vec<String>>>,
}

async fn page(State(s): State<Site>, Path(name): Path<String>) -> Response {
    s.hits.lock().unwrap().push(name.clone());
    let body = match name.as_str() {
        "foxes" => {
            "Foxes are small omnivores. The red fox is the largest of the true foxes and lives across the Northern Hemisphere."
        }
        "wolves" => {
            "Wolves live in packs. A grey wolf pack hunts large prey such as elk and moose across forests and tundra."
        }
        _ => {
            "Bears hibernate through the winter months, living off fat reserves built up during the autumn feeding season."
        }
    };
    ([(header::CONTENT_TYPE, "text/html")], article(&name, body)).into_response()
}

async fn image() -> Response {
    ([(header::CONTENT_TYPE, "image/png")], vec![0x89, b'P', b'N', b'G', 1, 2, 3]).into_response()
}

/// Search that "finds" the local site's pages.
struct LocalSearch {
    base: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for LocalSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }
    fn description(&self) -> &'static str {
        "search"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"query":{"type":"string"}}})
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let q = args["query"].as_str().unwrap_or("").to_lowercase();
        let names: &[&str] = if q.contains("bear") { &["bears"] } else { &["foxes", "wolves"] };
        let mut text = String::new();
        let mut hits = vec![];
        for name in names {
            let url = format!("{}/p/{name}", self.base);
            let title = format!("All about {name} animals");
            let n = ctx.add_source(&title, &url, &format!("{name} animals facts"), SourceKind::Search, None);
            text.push_str(&format!("[{n}] {title}\n    URL: {url}\n"));
            hits.push(
                json!({ "title": title, "url": url, "snippet": format!("{name} animals facts"), "engine": "local" }),
            );
        }
        Ok(ToolOutput::new(text, "results").with_data(json!({ "hits": hits })))
    }
}

struct World {
    app: Router,
    site: Site,
    searches: Arc<AtomicUsize>,
    dir: std::path::PathBuf,
    base: String,
}

async fn world() -> World {
    let site = Site::default();
    let app = Router::new().route("/p/{name}", get(page)).route("/img/{name}", get(image)).with_state(site.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = common::tmp_dir("followup");
    let (app, searches) = cookie_app(&base, &dir);
    World { app, site, searches, dir, base }
}

/// A cookie-web app over the given reports directory (a new one = a "restarted" server).
fn cookie_app(base: &str, dir: &std::path::Path) -> (Router, Arc<AtomicUsize>) {
    let mut cfg = Config::for_tests(dir.to_path_buf());
    cfg.allow_private_urls = true;
    let web = Arc::new(Web::new(Arc::new(cfg.clone())).unwrap());
    let searches = Arc::new(AtomicUsize::new(0));
    let mut tools = Registry::new();
    tools.add(LocalSearch { base: base.to_string(), calls: searches.clone() });
    tools.add(FetchPage { web: web.clone() });
    tools.web = Some(web);
    let llm = Arc::new(common::FakeLlm::new(true));
    let engine = common::engine_custom(llm, tools, cfg);
    (server::router(server::AppState::new(engine)), searches)
}

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::post(uri).header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_string())).unwrap()
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String, String) {
    let r = app.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let ctype = r.headers().get(header::CONTENT_TYPE).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    let body = String::from_utf8_lossy(&r.into_body().collect().await.unwrap().to_bytes()).into_owned();
    (status, ctype, body)
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
    let (s, _, b) = send(app, Request::get(uri).body(Body::empty()).unwrap()).await;
    (s, serde_json::from_str(&b).unwrap_or(Value::Null))
}

/// A first answer: pipeline-like standard research that reads both pages.
async fn first_report(w: &World) -> Value {
    let (s, _, b) = send(
        &w.app,
        post("/api/research", json!({ "query": "how do foxes and wolves live in the wild", "depth": "standard" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    serde_json::from_str(&b).unwrap()
}

#[tokio::test]
async fn sources_keep_their_full_content_and_images() {
    let w = world().await;
    let r = first_report(&w).await;
    let id = r["id"].as_str().unwrap();
    let fox = r["sources"].as_array().unwrap().iter().find(|s| s["url"].as_str().unwrap().ends_with("/foxes")).unwrap();
    let n = fox["n"].as_u64().unwrap();
    assert_eq!(fox["read"], true);

    let (s, list) = get_json(&w.app, &format!("/api/reports/{id}/sources")).await;
    assert_eq!(s, StatusCode::OK);
    let entry = list["sources"].as_array().unwrap().iter().find(|s| s["n"] == n).unwrap();
    assert_eq!(entry["stored"], true, "read pages are stored with the report: {entry}");
    assert_eq!(entry["images"], 2);

    let hits_before = w.site.hits.lock().unwrap().len();
    let (s, src) = get_json(&w.app, &format!("/api/reports/{id}/sources/{n}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(src["content"].as_str().unwrap().contains("The red fox is the largest"), "{src}");
    assert_eq!(src["images"][0]["url"], format!("{}/img/foxes.png", w.base));
    assert_eq!(src["images"][1]["alt"], "A diagram of foxes");
    assert_eq!(w.site.hits.lock().unwrap().len(), hits_before, "served from the stored copy, not re-fetched");

    // plain text and markdown views
    let (s, ctype, text) =
        send(&w.app, Request::get(format!("/api/reports/{id}/sources/{n}?format=text")).body(Body::empty()).unwrap())
            .await;
    assert_eq!(s, StatusCode::OK);
    assert!(ctype.starts_with("text/plain"));
    assert!(text.contains("The red fox") && !text.contains("**") && text.contains("Images:"), "{text}");
    let (_, ctype, md) = send(
        &w.app,
        Request::get(format!("/api/reports/{id}/sources/{n}?format=markdown&focus=red+fox&max_chars=600"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(ctype.starts_with("text/markdown"));
    assert!(md.starts_with("# foxes") && md.contains(&format!("Source [{n}]")), "{md}");

    // images come through the proxy
    let (s, ctype, png) =
        send(&w.app, Request::get(format!("/api/reports/{id}/sources/{n}/images/1")).body(Body::empty()).unwrap())
            .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ctype, "image/png");
    assert!(png.contains("PNG"));
    let (s, _) = get_json(&w.app, &format!("/api/reports/{id}/sources/{n}/images/9")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // everything at once
    let (s, all) = get_json(&w.app, &format!("/api/reports/{id}/content")).await;
    assert_eq!(s, StatusCode::OK);
    let contents: Vec<&str> = all["sources"].as_array().unwrap().iter().filter_map(|s| s["content"].as_str()).collect();
    assert!(contents.iter().any(|c| c.contains("red fox")) && contents.iter().any(|c| c.contains("grey wolf")));

    let (s, _) = get_json(&w.app, &format!("/api/reports/{id}/sources/99")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn follow_up_on_one_source_keeps_numbers_and_skips_the_web() {
    let w = world().await;
    let r = first_report(&w).await;
    let id = r["id"].as_str().unwrap();
    let wolf_n =
        r["sources"].as_array().unwrap().iter().find(|s| s["url"].as_str().unwrap().ends_with("/wolves")).unwrap()["n"]
            .as_u64()
            .unwrap();
    let searches_before = w.searches.load(Ordering::SeqCst);

    let (s, _, b) = send(
        &w.app,
        post(
            &format!("/api/reports/{id}/ask"),
            json!({ "question": format!("what do the wolves in @{wolf_n} hunt?") }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let f: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(f["follows"], id);
    assert_eq!(f["query"], format!("what do the wolves in @{wolf_n} hunt?"), "kept as asked");
    assert_eq!(f["triage"]["decided_by"], "follow-up");
    assert_eq!(w.searches.load(Ordering::SeqCst), searches_before, "a named source needs no new search");
    // same numbering as the parent
    for (a, b) in r["sources"].as_array().unwrap().iter().zip(f["sources"].as_array().unwrap()) {
        assert_eq!(a["n"], b["n"]);
        assert_eq!(a["url"], b["url"]);
    }
    let wolf = f["sources"].as_array().unwrap().iter().find(|s| s["n"] == wolf_n).unwrap();
    assert_eq!(wolf["read"], true);

    // the follow-up is a report too, and can be followed up
    let fid = f["id"].as_str().unwrap();
    let (s, _) = get_json(&w.app, &format!("/api/reports/{fid}/sources/{wolf_n}")).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn follow_up_searches_when_the_sources_dont_cover_it() {
    let w = world().await;
    let r = first_report(&w).await;
    let id = r["id"].as_str().unwrap();
    let parent_count = r["sources"].as_array().unwrap().len();
    let before = w.searches.load(Ordering::SeqCst);
    let (s, _, b) =
        send(&w.app, post("/api/research", json!({ "query": "do bears hibernate in winter", "report": id }))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let f: Value = serde_json::from_str(&b).unwrap();
    assert!(w.searches.load(Ordering::SeqCst) > before, "searched the web");
    let bears =
        f["sources"].as_array().unwrap().iter().find(|s| s["url"].as_str().unwrap().ends_with("/bears")).unwrap();
    assert!(bears["n"].as_u64().unwrap() as usize > parent_count, "new sources are numbered after the parent's");

    // web: never stays within the report
    let before = w.searches.load(Ordering::SeqCst);
    let (s, _, _) = send(
        &w.app,
        post("/api/research", json!({ "query": "do bears hibernate in winter", "report": id, "web": "never" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(w.searches.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn follow_ups_work_after_a_restart_from_stored_pages() {
    let w = world().await;
    let r = first_report(&w).await;
    let id = r["id"].as_str().unwrap();
    // a fresh server over the same reports directory: empty caches
    let (app2, _) = cookie_app(&w.base, &w.dir);
    let hits_before = w.site.hits.lock().unwrap().len();
    let (s, _, b) =
        send(&app2, post(&format!("/api/reports/{id}/ask"), json!({ "question": "tell me more", "web": "never" })))
            .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(w.site.hits.lock().unwrap().len(), hits_before, "answered from the stored pages");
}

#[tokio::test]
async fn follow_up_input_is_validated() {
    let w = world().await;
    let r = first_report(&w).await;
    let id = r["id"].as_str().unwrap();
    let (s, _, b) =
        send(&w.app, post(&format!("/api/reports/{id}/ask"), json!({ "question": "what about @42?" }))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert!(b.contains("no source [42]"));
    let (s, _, _) = send(&w.app, post("/api/reports/nope-123/ask", json!({ "question": "x" }))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = send(&w.app, post("/api/research", json!({ "query": "x", "sources": [1] }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _, _) = send(&w.app, post("/api/research", json!({ "query": "x", "report": id, "web": "maybe" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
