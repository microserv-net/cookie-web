//! Test doubles: a scripted LLM and offline web tools.
#![allow(dead_code)]

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use cookie_web::{
    config::Config,
    engine::Engine,
    llm::{ChatModel, ChatRequest, Completion, Message, ModelInfo, ToolCall},
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
    /// Fail this many writing attempts (after streaming a few tokens) before succeeding.
    pub write_failures: AtomicUsize,
    /// Native tool-calling rounds return an error (like a malformed tool call).
    pub native_broken: AtomicBool,
    /// Every call fails, as if Ollama were down.
    pub down: AtomicBool,
    /// Writing panics.
    pub panic_on_write: AtomicBool,
    /// Writing takes this long.
    pub write_delay: Mutex<Duration>,
    /// The writer's reply.
    pub answer: Mutex<String>,
    /// Fact-check prompts received (passage + statement).
    pub judged: Mutex<Vec<String>>,
    /// The last prompt the writer received.
    pub write_prompt: Mutex<String>,
}

impl FakeLlm {
    pub fn new(native_tools: bool) -> Self {
        Self {
            native_tools,
            calls: AtomicUsize::new(0),
            log: Mutex::new(vec![]),
            write_failures: AtomicUsize::new(0),
            native_broken: AtomicBool::new(false),
            down: AtomicBool::new(false),
            panic_on_write: AtomicBool::new(false),
            write_delay: Mutex::new(Duration::ZERO),
            answer: Mutex::new(ANSWER.to_string()),
            judged: Mutex::new(vec![]),
            write_prompt: Mutex::new(String::new()),
        }
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
        if self.down.load(Ordering::SeqCst) {
            anyhow::bail!("cannot reach Ollama (fake outage)");
        }
        let sys = req.messages.first().map(|m| m.content.clone()).unwrap_or_default();
        if sys.contains("You check one statement") {
            // A stand-in fact checker that knows the right pairings:
            // F.R.I.D.A.Y. = Allison Hogan (Orla Brady), E.D.I.T.H. = Edith Marks (Emily Hampshire).
            self.note("judge");
            let user = req.messages.last().map(|m| m.content.clone()).unwrap_or_default();
            self.judged.lock().unwrap().push(user.clone());
            let stmt = user.rsplit("Statement:").next().unwrap_or("").to_lowercase();
            let crossed = stmt.contains("friday") && (stmt.contains("edith marks") || stmt.contains("hampshire"));
            let verdict = if crossed { "contradicted" } else { "supported" };
            return Ok(Message::assistant(format!(r#"{{"verdict":"{verdict}"}}"#)));
        }
        if sys.contains("You help a web search that found nothing") {
            self.note("rephrase");
            return Ok(Message::assistant(r#"{"queries":["rescued query","another angle"]}"#));
        }
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
                if self.native_broken.load(Ordering::SeqCst) {
                    anyhow::bail!("Ollama error (500): error parsing tool call");
                }
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
        req: ChatRequest,
        on_token: &(dyn for<'t> Fn(&'t str) + Send + Sync),
    ) -> anyhow::Result<Completion> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.note("write");
        let delay = *self.write_delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if self.panic_on_write.load(Ordering::SeqCst) {
            panic!("fake writer panic");
        }
        if self.down.load(Ordering::SeqCst) {
            anyhow::bail!("cannot reach Ollama (fake outage)");
        }
        if self.write_failures.load(Ordering::SeqCst) > 0 {
            self.write_failures.fetch_sub(1, Ordering::SeqCst);
            on_token("Half an ans");
            anyhow::bail!("Ollama stream interrupted (fake)");
        }
        // Like a real writer, cite the numbers the evidence actually shows: [1][2][3] in the scripted
        // answer stand for the first three numbers in the prompt.
        let prompt = req.messages.last().map(|m| m.content.clone()).unwrap_or_default();
        *self.write_prompt.lock().unwrap() = prompt.clone();
        let mut shown: Vec<String> = vec![];
        for cap in prompt.split('[').skip(1) {
            if let Some(n) = cap.split(']').next().filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                && !shown.contains(&n.to_string())
            {
                shown.push(n.to_string());
            }
        }
        let mut answer = self.answer.lock().unwrap().clone();
        for (i, n) in shown.iter().take(3).enumerate() {
            answer = answer.replace(&format!("[{}]", i + 1), &format!("[@{n}]"));
        }
        let answer = answer.replace("[@", "[");
        for chunk in answer.split_inclusive(' ') {
            on_token(chunk);
        }
        Ok(Completion::new(answer))
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
        let raw = args["query"].as_str().unwrap_or("");
        if raw.contains("nothing") {
            return Ok(ToolOutput::new("No results.", "no results").with_data(json!({ "hits": [] })));
        }
        let q = raw.replace(' ', "-");
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

/// Offline page reader; URLs containing `.0` (if set) fail like dead links.
pub struct FakeFetch(pub Option<&'static str>);

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
        if self.0.is_some_and(|bad| url.contains(bad)) {
            ctx.refund_fetch();
            anyhow::bail!("HTTP 404 for {url}");
        }
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
    r.add(FakeFetch(None));
    r.add(FakeWiki);
    r
}

/// A tool that panics, to prove one broken tool can't take a job down.
pub struct PanicTool(pub &'static str);

#[async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> &'static str {
        "panics"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        panic!("tool exploded")
    }
}

pub fn engine_custom(llm: Arc<FakeLlm>, tools: Registry, cfg: Config) -> Arc<Engine> {
    Arc::new(Engine::new(Arc::new(cfg), llm, Arc::new(tools)))
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
