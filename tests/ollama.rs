//! The real Ollama client against a scripted fake Ollama server: retries, stream integrity, errors.
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use cookie_web::{
    config::Config,
    llm::{ChatModel, ChatRequest, Message, ollama::Ollama},
};
use serde_json::{Value, json};

/// What the fake server does on the n-th `/api/chat` call (the last entry repeats).
#[derive(Clone)]
enum Reply {
    Status(u16, &'static str),
    Chat(&'static str),
    /// NDJSON stream lines, sent verbatim.
    Stream(Vec<Value>),
}

#[derive(Clone)]
struct Fake {
    script: Arc<Vec<Reply>>,
    calls: Arc<AtomicUsize>,
    bodies: Arc<std::sync::Mutex<Vec<Value>>>,
}

async fn chat(State(f): State<Fake>, Json(body): Json<Value>) -> Response {
    f.bodies.lock().unwrap().push(body);
    let i = f.calls.fetch_add(1, Ordering::SeqCst);
    match f.script.get(i).or(f.script.last()).cloned().unwrap() {
        Reply::Status(code, err) => {
            (StatusCode::from_u16(code).unwrap(), Json(json!({ "error": err }))).into_response()
        }
        Reply::Chat(text) => Json(json!({ "message": { "role": "assistant", "content": text }, "done": true,
                         "prompt_eval_count": 1200, "eval_count": 40, "total_duration": 9_500_000_000u64 }))
        .into_response(),
        Reply::Stream(lines) => {
            let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
            ([("content-type", "application/x-ndjson")], body).into_response()
        }
    }
}

async fn show() -> Json<Value> {
    Json(json!({ "capabilities": ["completion", "tools"] }))
}

async fn fake_ollama(script: Vec<Reply>) -> (Ollama, Arc<AtomicUsize>) {
    let (o, calls, _) = fake_ollama_with(script, |_| {}).await;
    (o, calls)
}

async fn fake_ollama_with(
    script: Vec<Reply>,
    tweak: impl FnOnce(&mut Config),
) -> (Ollama, Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<Value>>>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(std::sync::Mutex::new(vec![]));
    let state = Fake { script: Arc::new(script), calls: calls.clone(), bodies: bodies.clone() };
    let app = Router::new().route("/api/chat", post(chat)).route("/api/show", post(show)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut cfg = Config::for_tests("reports".into());
    cfg.ollama_host = format!("http://{addr}");
    tweak(&mut cfg);
    let o = Ollama::new(Arc::new(cfg)).with_backoff(vec![Duration::from_millis(5); 3]);
    (o, calls, bodies)
}

fn req() -> ChatRequest {
    ChatRequest::new(vec![Message::user("hi")])
}

fn token(t: &str) -> Value {
    json!({ "message": { "role": "assistant", "content": t }, "done": false })
}

fn done(reason: &str) -> Value {
    json!({ "message": { "role": "assistant", "content": "" }, "done": true, "done_reason": reason,
            "prompt_eval_count": 3000, "eval_count": 2, "total_duration": 61_000_000_000u64 })
}

#[tokio::test]
async fn reports_usage_and_passes_the_thread_cap() {
    let (o, _, bodies) = fake_ollama_with(vec![Reply::Chat("hi")], |c| c.num_thread = Some(3)).await;
    let m = o.chat(req()).await.unwrap();
    assert_eq!(m.usage.prompt_tokens, 1200);
    assert_eq!(m.usage.output_tokens, 40);
    assert_eq!(m.usage.model_ms, 9_500);
    assert_eq!(bodies.lock().unwrap()[0]["options"]["num_thread"], 3);

    let (o, _) = fake_ollama(vec![Reply::Stream(vec![token("a"), token("b"), done("stop")])]).await;
    let c = o.chat_stream(req(), &|_| {}).await.unwrap();
    assert_eq!(c.usage.prompt_tokens, 3000);
    assert_eq!(c.usage.model_ms, 61_000);
}

#[tokio::test]
async fn retries_while_ollama_is_busy_or_restarting() {
    let (o, calls) = fake_ollama(vec![
        Reply::Status(503, "server busy"),
        Reply::Status(500, "llama runner process has terminated"),
        Reply::Chat("hello"),
    ])
    .await;
    let m = o.chat(req()).await.unwrap();
    assert_eq!(m.content, "hello");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn gives_up_after_the_retry_budget() {
    let (o, calls) = fake_ollama(vec![Reply::Status(503, "server busy")]).await;
    assert!(o.chat(req()).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 4, "1 try + 3 retries");
}

#[tokio::test]
async fn bad_tool_calls_and_missing_models_are_not_retried() {
    let (o, calls) = fake_ollama(vec![Reply::Status(500, "error parsing tool call: invalid character")]).await;
    assert!(o.chat(req()).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the agent switches protocol instead");

    let (o, calls) = fake_ollama(vec![Reply::Status(404, "model \"nope\" not found, try pulling it first")]).await;
    let err = o.chat(req()).await.unwrap_err();
    assert!(format!("{err}").contains("ollama pull"), "{err}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stream_reports_truncation() {
    let (o, _) = fake_ollama(vec![Reply::Stream(vec![token("Hello "), token("world"), done("length")])]).await;
    let c = o.chat_stream(req(), &|_| {}).await.unwrap();
    assert_eq!(c.text, "Hello world");
    assert!(c.truncated);
}

#[tokio::test]
async fn stream_retries_before_any_token_was_sent() {
    let (o, calls) =
        fake_ollama(vec![Reply::Status(503, "busy"), Reply::Stream(vec![token("ok"), done("stop")])]).await;
    let seen = std::sync::Mutex::new(String::new());
    let c = o.chat_stream(req(), &|t| seen.lock().unwrap().push_str(t)).await.unwrap();
    assert_eq!(c.text, "ok");
    assert!(!c.truncated);
    assert_eq!(*seen.lock().unwrap(), "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_stream_cut_off_midway_is_an_error_not_a_short_answer() {
    // tokens arrive, then the connection ends without `done`
    let (o, calls) = fake_ollama(vec![Reply::Stream(vec![token("Half an "), token("answ")])]).await;
    let err = o.chat_stream(req(), &|_| {}).await.unwrap_err();
    assert!(format!("{err}").contains("ended before"), "{err}");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "tokens were already shown; the engine decides what to do");
}

#[tokio::test]
async fn unreachable_ollama_is_retried_then_reported() {
    let mut cfg = Config::for_tests("reports".into());
    cfg.ollama_host = "http://127.0.0.1:9".into(); // discard port: connection refused
    let o = Ollama::new(Arc::new(cfg)).with_backoff(vec![Duration::from_millis(5); 2]);
    let err = o.chat(req()).await.unwrap_err();
    assert!(format!("{err:#}").contains("cannot reach Ollama"), "{err:#}");
}
