//! Test doubles: a scripted LLM and offline web tools.
#![allow(dead_code)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use cookie_web::{
    config::Config,
    engine::Engine,
    llm::{ChatModel, ChatRequest, Message, ModelInfo, ToolCall},
    sources::SourceKind,
    tools::{Registry, Tool, ToolCtx, ToolOutput},
};
use serde_json::{Value, json};

pub const ANSWER: &str =
    "VisionQuest is a test subject [1]. More detail here [2][3]. Bogus cite [99].\n\n## Sources\n- [1] whatever\n";

pub struct FakeLlm {
    pub native_tools: bool,
    pub calls: AtomicUsize,
    pub log: Mutex<Vec<String>>,
}

impl FakeLlm {
    pub fn new(native_tools: bool) -> Self {
        Self { native_tools, calls: AtomicUsize::new(0), log: Mutex::new(vec![]) }
    }
    fn note(&self, s: &str) {
        self.log.lock().unwrap().push(s.to_string());
    }
}

#[async_trait]
impl ChatModel for FakeLlm {
    fn name(&self) -> String {
        "fake-model".into()
    }

    async fn chat(&self, req: ChatRequest) -> anyhow::Result<Message> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let sys = req.messages.first().map(|m| m.content.clone()).unwrap_or_default();
        if sys.contains("You triage") {
            self.note("triage");
            return Ok(Message::assistant(
                r#"{"depth":"standard","intent":"fact","search_query":"fake query","reason":"test"}"#,
            ));
        }
        if sys.contains("You plan web research") {
            self.note("plan");
            return Ok(Message::assistant(r#"{"sub_questions":["q1","q2"],"queries":["alpha","beta","gamma"]}"#));
        }
        if sys.contains("You review research coverage") {
            self.note("gaps");
            return Ok(Message::assistant(r#"{"enough":false,"follow_up_queries":["delta"]}"#));
        }
        if sys.contains("You extract research notes") {
            self.note("notes");
            return Ok(Message::assistant("- a concrete fact\n- another fact"));
        }
        if sys.contains("research planner") {
            let fetched = req
                .messages
                .iter()
                .any(|m| m.content.contains("Result of fetch_page") || m.tool_name.as_deref() == Some("fetch_page"));
            if !req.tools.is_empty() {
                self.note("agent-native");
                if fetched {
                    return Ok(Message::assistant("DONE"));
                }
                let mut m = Message::assistant("");
                m.tool_calls =
                    vec![ToolCall::new("fetch_page", json!({ "url": "https://example.com/a", "focus": "x" }))];
                return Ok(m);
            }
            self.note("agent-json");
            if fetched {
                return Ok(Message::assistant(r#"{"tool":"done","args":{}}"#));
            }
            return Ok(Message::assistant(r#"{"tool":"fetch_page","args":{"url":"https://example.com/b"}}"#));
        }
        Ok(Message::assistant("ok"))
    }

    async fn chat_stream(
        &self,
        _req: ChatRequest,
        on_token: &(dyn for<'t> Fn(&'t str) + Send + Sync),
    ) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.note("write");
        for chunk in ANSWER.split_inclusive(' ') {
            on_token(chunk);
        }
        Ok(ANSWER.to_string())
    }

    async fn supports_tools(&self, _model: Option<&str>) -> bool {
        self.native_tools
    }

    async fn info(&self) -> ModelInfo {
        ModelInfo {
            reachable: true,
            model: "fake-model".into(),
            available: true,
            supports_tools: self.native_tools,
            ..Default::default()
        }
    }
}

/// Offline search: three deterministic hits per query.
pub struct FakeSearch(pub &'static str);

#[async_trait]
impl Tool for FakeSearch {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> &'static str {
        "fake search"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]})
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        let q = args["query"].as_str().unwrap_or("").replace(' ', "-");
        let hits: Vec<Value> = (1..=3)
            .map(|i| json!({ "title": format!("{q} result {i}"), "url": format!("https://site{i}.example/{q}"), "snippet": format!("snippet {i}"), "engine": "fake" }))
            .collect();
        let mut text = String::new();
        for h in &hits {
            let n = ctx.add_source(
                h["title"].as_str().unwrap(),
                h["url"].as_str().unwrap(),
                "snippet",
                SourceKind::Search,
                None,
            );
            text.push_str(&format!("[{n}] {}\n", h["title"].as_str().unwrap()));
        }
        Ok(ToolOutput::new(text, "3 results").with_data(json!({ "hits": hits })))
    }
}

pub struct FakeFetch;

#[async_trait]
impl Tool for FakeFetch {
    fn name(&self) -> &'static str {
        "fetch_page"
    }
    fn description(&self) -> &'static str {
        "fake fetch"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]})
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        if !ctx.take_fetch() {
            return Ok(ToolOutput::new("budget exhausted", "budget exhausted"));
        }
        let url = args["url"].as_str().unwrap_or("");
        let n = ctx.add_source("Fetched page", url, "", SourceKind::Page, None);
        Ok(ToolOutput::new(format!("[{n}] Fetched page\nURL: {url}\n\nLots of useful content."), format!("read [{n}]")))
    }
}

pub struct FakeWiki;

#[async_trait]
impl Tool for FakeWiki {
    fn name(&self) -> &'static str {
        "wikipedia"
    }
    fn description(&self) -> &'static str {
        "fake wiki"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"topic":{"type":"string"}}})
    }
    async fn call(&self, _args: Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        let n = ctx.add_source(
            "Thing — Wikipedia",
            "https://en.wikipedia.org/wiki/Thing",
            "A thing.",
            SourceKind::Wiki,
            None,
        );
        Ok(ToolOutput::new(format!("[{n}] Thing — Wikipedia\nA thing."), format!("[{n}] Thing")))
    }
}

pub fn fake_registry() -> Registry {
    let mut r = Registry::new();
    r.add(FakeSearch("web_search"));
    r.add(FakeSearch("news_search"));
    r.add(FakeFetch);
    r.add(FakeWiki);
    r
}

pub fn tmp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cookie-web-test-{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn engine(native_tools: bool, name: &str) -> (Arc<Engine>, Arc<FakeLlm>) {
    engine_with(native_tools, Config::for_tests(tmp_dir(name)))
}

pub fn engine_with(native_tools: bool, cfg: Config) -> (Arc<Engine>, Arc<FakeLlm>) {
    let llm = Arc::new(FakeLlm::new(native_tools));
    let e = Engine::new(Arc::new(cfg), llm.clone(), Arc::new(fake_registry()));
    (Arc::new(e), llm)
}
