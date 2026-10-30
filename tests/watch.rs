//! The watchdog: registration, heartbeats, its own health probes, and what it reports.
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use cookie_web::watch::{Beat, Status, Watch, WatchConfig};
use serde_json::{Value, json};

/// A stand-in cookie-web: 0 = healthy, 1 = degraded, 2 = errors, 3 = hangs.
async fn fake_cookie_web() -> (String, Arc<AtomicU8>) {
    let mode = Arc::new(AtomicU8::new(0));
    let app = Router::new()
        .route(
            "/api/health",
            get(|State(m): State<Arc<AtomicU8>>| async move {
                match m.load(Ordering::SeqCst) {
                    0 => Json(json!({ "status": "ok", "version": "9.9.9" })).into_response(),
                    1 => Json(json!({ "status": "degraded", "version": "9.9.9" })).into_response(),
                    2 => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                    _ => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        StatusCode::OK.into_response()
                    }
                }
            }),
        )
        .with_state(mode.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (base, mode)
}

fn watch(target: &str) -> Arc<Watch> {
    Watch::new(WatchConfig {
        target: target.into(),
        api_key: Some("k".into()),
        password: None,
        interval: Duration::from_secs(60),
    })
}

fn beat(pid: u32) -> Beat {
    Beat {
        pid,
        name: if pid == std::process::id() { Beat::own_name() } else { None },
        version: "9.9.9".into(),
        port: 1,
        started_at: chrono::Utc::now(),
        jobs_running: 1,
        jobs_queued: 2,
        jobs_paused: 0,
    }
}

#[tokio::test]
async fn online_degraded_unresponsive_and_back() {
    let (base, mode) = fake_cookie_web().await;
    let w = watch(&base);
    w.beat(beat(std::process::id()));
    w.check_now().await;
    assert_eq!(w.status(), Status::Online);
    mode.store(1, Ordering::SeqCst);
    w.check_now().await;
    assert_eq!(w.status(), Status::Degraded);
    // the process is alive (it's us) but the API fails: one miss is tolerated, two are not
    mode.store(2, Ordering::SeqCst);
    w.check_now().await;
    assert_eq!(w.status(), Status::Degraded);
    w.check_now().await;
    assert_eq!(w.status(), Status::Unresponsive);
    mode.store(0, Ordering::SeqCst);
    w.check_now().await;
    assert_eq!(w.status(), Status::Online);
    let r = w.report(true);
    assert_eq!(r["detail"]["jobs_queued"], 2);
    let changes: Vec<&str> = r["incidents"].as_array().unwrap().iter().map(|i| i["to"].as_str().unwrap()).collect();
    assert_eq!(changes, ["online", "unresponsive", "degraded", "online"]);
}

#[tokio::test]
async fn a_dead_process_is_offline_at_once() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let w = watch("http://127.0.0.1:9"); // nothing listens there
    w.beat(beat(pid));
    w.check_now().await;
    assert_eq!(w.status(), Status::Offline);
    assert!(w.report(false)["reason"].as_str().unwrap().contains("gone"));
}

#[tokio::test]
async fn a_clean_shutdown_says_so_and_a_restart_registers_again() {
    let (base, mode) = fake_cookie_web().await;
    let w = watch(&base);
    w.beat(beat(4_000_000)); // a pid that doesn't exist: only the probe and the bye matter
    w.check_now().await;
    assert_eq!(w.status(), Status::Online);
    mode.store(2, Ordering::SeqCst);
    w.bye(4_000_000);
    assert_eq!(w.status(), Status::Offline);
    assert!(w.report(false)["reason"].as_str().unwrap().contains("clean shutdown"));
    mode.store(0, Ordering::SeqCst);
    w.beat(beat(std::process::id()));
    w.check_now().await;
    assert_eq!(w.status(), Status::Online);
}

#[tokio::test]
async fn nothing_running_is_offline_not_unknown() {
    let w = watch("http://127.0.0.1:9");
    assert_eq!(w.status(), Status::Unknown);
    w.check_now().await;
    w.check_now().await;
    assert_eq!(w.status(), Status::Offline);
}

async fn serve_watch(w: Arc<Watch>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let app = cookie_web::watch::router(w).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    base
}

#[tokio::test]
async fn http_status_page_heartbeats_and_privacy() {
    let (target, _) = fake_cookie_web().await;
    let w = watch(&target);
    let base = serve_watch(w.clone()).await;
    let http = reqwest::Client::new();

    // heartbeat over HTTP from this machine
    let r = http.post(format!("{base}/watch/beat")).json(&beat(std::process::id())).send().await.unwrap();
    assert!(r.status().is_success());
    // …but not through the proxy
    let r =
        http.post(format!("{base}/watch/beat")).header("x-real-ip", "203.0.113.9").json(&beat(1)).send().await.unwrap();
    assert_eq!(r.status(), 403);

    w.check_now().await;
    let r = http.get(format!("{base}/watch/status")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!((v["status"].as_str(), v["api"].as_str()), (Some("online"), Some("up")));
    assert!(v.get("detail").is_none(), "no process details without signing in");
    let v: Value =
        http.get(format!("{base}/watch/status")).bearer_auth("k").send().await.unwrap().json().await.unwrap();
    assert_eq!(v["detail"]["pid"], std::process::id());

    // the status page is served, and the watchdog's own liveness check
    let page = http.get(format!("{base}/watch/")).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("/watch/status"));
    assert_eq!(http.get(format!("{base}/watch/alive")).send().await.unwrap().text().await.unwrap(), "ok");

    // down = 503, so plain uptime monitors notice
    w.bye(std::process::id() + 1); // a different pid's bye is ignored
    assert_eq!(w.status(), Status::Online);
}

#[tokio::test]
async fn cookie_web_registers_itself() {
    let w = watch("http://127.0.0.1:9");
    let base = serve_watch(w.clone()).await;
    cookie_web::watch::start_heartbeat(base.clone(), || beat(std::process::id()));
    for _ in 0..100 {
        if w.report(true)["detail"]["pid"] == std::process::id() {
            cookie_web::watch::say_bye(&base).await;
            assert!(w.report(false)["reason"].as_str().unwrap().contains("clean shutdown"));
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no heartbeat arrived");
}
