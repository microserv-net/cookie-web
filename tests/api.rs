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
