mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use cookie_web::{config::Config, server};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app_with(cfg: Config) -> axum::Router {
    let (engine, _) = common::engine_with(true, cfg);
    server::router(server::AppState::new(engine))
}

fn app() -> axum::Router {
    app_with(Config::for_tests(common::tmp_dir("api")))
}

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::post(uri).header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_string())).unwrap()
}

async fn body_string(resp: axum::response::Response) -> String {
    String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

#[tokio::test]
async fn research_returns_json_result() {
    let resp = app().oneshot(post("/api/research", json!({ "query": "VisionQuest" }))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().contains_key("x-cookie-job"));
    let v: Value = serde_json::from_str(&body_string(resp).await).unwrap();
    assert_eq!(v["depth"], "quick");
    assert!(v["markdown"].as_str().unwrap().contains("## Sources"));
    assert!(v["sources"].as_array().unwrap().len() >= 3);
    assert!(v["id"].as_str().unwrap().contains("visionquest"));
}

#[tokio::test]
async fn research_can_return_markdown() {
    let resp = app().oneshot(post("/api/research?format=markdown", json!({ "query": "VisionQuest" }))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/markdown"));
    assert!(body_string(resp).await.contains("VisionQuest is a test subject"));
}

#[tokio::test]
async fn validation_errors_are_400() {
    let a = app();
    let r = a.clone().oneshot(post("/api/research", json!({ "query": "" }))).await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = a.oneshot(post("/api/research", json!({ "query": "x", "depth": "huge" }))).await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(r).await.contains("depth"));
}

#[tokio::test]
async fn stream_emits_sse_events_in_order() {
    let resp = app().oneshot(post("/api/research/stream", json!({ "query": "VisionQuest" }))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/event-stream"));
    let body = body_string(resp).await;
    let order: Vec<&str> = body.lines().filter_map(|l| l.strip_prefix("event: ")).collect();
    assert_eq!(order.first(), Some(&"started"));
    assert_eq!(order.last(), Some(&"done"));
    for e in ["triage", "tool_call", "tool_result", "source", "token"] {
        assert!(order.contains(&e), "missing {e} in {order:?}");
    }
}

#[tokio::test]
async fn async_jobs_and_reports() {
    let a = app();
    let r = a.clone().oneshot(post("/api/jobs", json!({ "query": "VisionQuest" }))).await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let job: Value = serde_json::from_str(&body_string(r).await).unwrap();
    let id = job["id"].as_str().unwrap().to_string();

    let mut done = Value::Null;
    for _ in 0..100 {
        let r = a.clone().oneshot(Request::get(format!("/api/jobs/{id}")).body(Body::empty()).unwrap()).await.unwrap();
        let v: Value = serde_json::from_str(&body_string(r).await).unwrap();
        if v["status"] == "done" {
            done = v;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let report_id = done["result"]["id"].as_str().expect("job finished").to_string();

    // replayed events after completion
    let r =
        a.clone().oneshot(Request::get(format!("/api/jobs/{id}/events")).body(Body::empty()).unwrap()).await.unwrap();
    assert!(body_string(r).await.contains("event: done"));

    let r = a.clone().oneshot(Request::get("/api/reports").body(Body::empty()).unwrap()).await.unwrap();
    let list: Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(list["reports"][0]["id"], report_id.as_str());

    let r = a
        .clone()
        .oneshot(Request::get(format!("/api/reports/{report_id}/markdown")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(body_string(r).await.starts_with("---\nid: "));

    let r = a
        .clone()
        .oneshot(Request::delete(format!("/api/reports/{report_id}")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = a.oneshot(Request::get("/api/reports/../../etc/passwd").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_key_is_enforced() {
    let mut cfg = Config::for_tests(common::tmp_dir("auth"));
    cfg.api_key = Some("s3cret".into());
    let a = app_with(cfg);
    let r = a.clone().oneshot(post("/api/research", json!({ "query": "VisionQuest" }))).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = a.clone().oneshot(Request::get("/api/health").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let mut req = post("/api/research", json!({ "query": "VisionQuest" }));
    req.headers_mut().insert(header::AUTHORIZATION, "Bearer s3cret".parse().unwrap());
    assert_eq!(a.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
    let r = a.oneshot(Request::get("/api/tools?api_key=s3cret").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn gui_and_manifest_are_served() {
    let a = app();
    let r = a.clone().oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    assert!(body_string(r).await.contains("cookie"));
    let r = a.clone().oneshot(Request::get("/app.js").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = a.oneshot(Request::get("/api/manifest").body(Body::empty()).unwrap()).await.unwrap();
    let v: Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["name"], "cookie-web");
    assert!(v["endpoints"]["stream"]["path"].is_string());
}

// ------------------------------------------------------------ reliability

fn slow_app(delay_ms: u64, max_concurrent: usize) -> (axum::Router, std::sync::Arc<common::FakeLlm>) {
    let mut cfg = Config::for_tests(common::tmp_dir("slow"));
    cfg.max_concurrent = max_concurrent;
    let llm = std::sync::Arc::new(common::FakeLlm::new(true));
    *llm.write_delay.lock().unwrap() = std::time::Duration::from_millis(delay_ms);
    let engine = common::engine_custom(llm.clone(), common::fake_registry(), cfg);
    (server::router(server::AppState::new(engine)), llm)
}

async fn get_json(a: &axum::Router, uri: &str) -> Value {
    let r = a.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
    serde_json::from_str(&body_string(r).await).unwrap()
}

async fn wait_status(a: &axum::Router, id: &str, want: &str) -> Value {
    for _ in 0..200 {
        let v = get_json(a, &format!("/api/jobs/{id}")).await;
        if v["status"] == want {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("job {id} never reached {want}");
}

#[tokio::test]
async fn jobs_can_be_cancelled() {
    let (a, _) = slow_app(10_000, 1);
    let r = a.clone().oneshot(post("/api/jobs", json!({ "query": "VisionQuest" }))).await.unwrap();
    let id = serde_json::from_str::<Value>(&body_string(r).await).unwrap()["id"].as_str().unwrap().to_string();
    wait_status(&a, &id, "running").await;
    let r = a.clone().oneshot(Request::delete(format!("/api/jobs/{id}")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let v = wait_status(&a, &id, "cancelled").await;
    assert_eq!(v["error"], "cancelled");
    // the slot is free again: a new job runs (and is cancelled) right away
    let r = a.clone().oneshot(post("/api/jobs", json!({ "query": "VisionQuest" }))).await.unwrap();
    let id2 = serde_json::from_str::<Value>(&body_string(r).await).unwrap()["id"].as_str().unwrap().to_string();
    wait_status(&a, &id2, "running").await;
    a.oneshot(Request::delete(format!("/api/jobs/{id2}")).body(Body::empty()).unwrap()).await.unwrap();
}

#[tokio::test]
async fn idempotency_key_reattaches_to_the_same_job() {
    let a = app();
    let mut ids = vec![];
    for _ in 0..2 {
        let mut req = post("/api/jobs", json!({ "query": "VisionQuest" }));
        req.headers_mut().insert("idempotency-key", "supervisor-task-42".parse().unwrap());
        let r = a.clone().oneshot(req).await.unwrap();
        ids.push(serde_json::from_str::<Value>(&body_string(r).await).unwrap()["id"].as_str().unwrap().to_string());
    }
    assert_eq!(ids[0], ids[1]);
    let r = a.clone().oneshot(post("/api/jobs", json!({ "query": "VisionQuest" }))).await.unwrap();
    let other = serde_json::from_str::<Value>(&body_string(r).await).unwrap()["id"].as_str().unwrap().to_string();
    assert_ne!(other, ids[0], "no key, new job");
}

#[tokio::test]
async fn event_stream_resumes_where_the_client_left_off() {
    let a = app();
    let r = a.clone().oneshot(post("/api/jobs", json!({ "query": "VisionQuest" }))).await.unwrap();
    let id = serde_json::from_str::<Value>(&body_string(r).await).unwrap()["id"].as_str().unwrap().to_string();
    wait_status(&a, &id, "done").await;
    let r = a
        .clone()
        .oneshot(Request::get(format!("/api/jobs/{id}/events?from=3")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = body_string(r).await;
    let ids: Vec<usize> = body.lines().filter_map(|l| l.strip_prefix("id: ")).map(|n| n.parse().unwrap()).collect();
    assert_eq!(ids.first(), Some(&4));
    assert!(body.contains("event: done"));
    let mut req = Request::get(format!("/api/jobs/{id}/events")).body(Body::empty()).unwrap();
    req.headers_mut().insert("last-event-id", "5".parse().unwrap());
    let body = body_string(a.oneshot(req).await.unwrap()).await;
    assert!(body.lines().find_map(|l| l.strip_prefix("id: ")) == Some("6"));
}

#[tokio::test]
async fn a_crashing_job_reports_an_error_instead_of_hanging() {
    let (a, llm) = slow_app(0, 1);
    llm.panic_on_write.store(true, std::sync::atomic::Ordering::SeqCst);
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        a.oneshot(post("/api/research", json!({ "query": "VisionQuest" }))),
    )
    .await
    .expect("no hang")
    .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    assert!(body_string(r).await.contains("internal error"));
}

#[tokio::test]
async fn queued_jobs_say_they_are_waiting() {
    let (a, _) = slow_app(300, 1);
    let r = a.clone().oneshot(post("/api/jobs", json!({ "query": "VisionQuest" }))).await.unwrap();
    let first = serde_json::from_str::<Value>(&body_string(r).await).unwrap()["id"].as_str().unwrap().to_string();
    wait_status(&a, &first, "running").await;
    let r = a.clone().oneshot(post("/api/research/stream", json!({ "query": "VisionQuest" }))).await.unwrap();
    let body = body_string(r).await;
    assert!(body.contains("Queued"), "{body}");
    assert!(body.contains("event: done"));
}

#[tokio::test]
async fn health_reports_search_backend_state() {
    let v = get_json(&app(), "/api/health").await;
    assert!(v["search_health"].is_array());
    assert!(v["jobs_queued"].is_number());
}
