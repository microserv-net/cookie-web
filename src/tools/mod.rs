//! The tool belt. Each tool is described with a JSON schema (sent to the model
//! for native tool calling) and returns text for the model plus a short
//! summary for the UI. Tools register every URL they touch as a citable source.

pub mod crawl;
pub mod extract;
pub mod fetch;
pub mod net;
pub mod search;
pub mod wiki;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicIsize, Ordering},
};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    config::Config,
    events::{Event, Events},
    sources::{SourceKind, SourceRegistry},
};

#[derive(Clone, Debug)]
pub struct ToolOutput {
    /// What the model sees.
    pub text: String,
    /// One-line summary for humans (UI / logs).
    pub summary: String,
    /// Structured payload (optional).
    pub data: Value,
}

impl ToolOutput {
    pub fn new(text: impl Into<String>, summary: impl Into<String>) -> Self {
        Self { text: text.into(), summary: summary.into(), data: Value::Null }
    }
    pub fn with_data(mut self, d: Value) -> Self {
        self.data = d;
        self
    }
}

/// Per-job context shared by all tool calls.
#[derive(Clone)]
pub struct ToolCtx {
    pub sources: Arc<Mutex<SourceRegistry>>,
    pub events: Events,
    pub results_per_search: usize,
    pub page_chars: usize,
    fetches_left: Arc<AtomicIsize>,
}

impl ToolCtx {
    pub fn new(events: Events, results_per_search: usize, page_chars: usize, max_fetches: usize) -> Self {
        Self {
            sources: Arc::new(Mutex::new(SourceRegistry::new())),
            events,
            results_per_search,
            page_chars,
            fetches_left: Arc::new(AtomicIsize::new(max_fetches as isize)),
        }
    }

    /// Register a source, emit a `source` event, return its citation number.
    pub fn add_source(&self, title: &str, url: &str, snippet: &str, kind: SourceKind, date: Option<String>) -> usize {
        let (n, src) = {
            let mut reg = self.sources.lock().unwrap();
            let n = reg.add(title, url, snippet, kind);
            reg.set_date(n, date);
            (n, reg.get(n).cloned())
        };
        if let Some(source) = src {
            self.events.emit(Event::Source { source });
        }
        n
    }

    /// Consume one unit of the page-reading budget. Returns false if exhausted.
    pub fn take_fetch(&self) -> bool {
        self.fetches_left.fetch_sub(1, Ordering::SeqCst) > 0
    }

    pub fn fetches_left(&self) -> isize {
        self.fetches_left.load(Ordering::SeqCst).max(0)
    }

    pub fn add_fetch_budget(&self, n: usize) {
        let cur = self.fetches_left.load(Ordering::SeqCst).max(0);
        self.fetches_left.store(cur + n as isize, Ordering::SeqCst);
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;
    async fn call(&self, args: Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput>;

    /// Ollama / OpenAI-style function spec.
    fn spec(&self) -> Value {
        json!({
            "type": "function",
            "function": { "name": self.name(), "description": self.description(), "parameters": self.parameters() }
        })
    }
}

#[derive(Clone, Default)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The standard web tool belt.
    pub fn standard(cfg: Arc<Config>) -> anyhow::Result<Self> {
        let web = Arc::new(net::Web::new(cfg.clone())?);
        let engine = Arc::new(search::SearchEngine::from_config(&cfg));
        let mut r = Self::new();
        r.add(search::WebSearchTool { web: web.clone(), engine: engine.clone(), kind: search::Kind::Web });
        r.add(search::WebSearchTool { web: web.clone(), engine: engine.clone(), kind: search::Kind::News });
        r.add(fetch::FetchPage { web: web.clone() });
        r.add(fetch::ExtractLinks { web: web.clone() });
        r.add(crawl::CrawlSite { web: web.clone() });
        r.add(wiki::Wikipedia { web });
        Ok(r)
    }

    pub fn add(&mut self, t: impl Tool + 'static) -> &mut Self {
        self.tools.retain(|x| x.name() != t.name());
        self.tools.push(Arc::new(t));
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    pub fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    pub fn specs(&self) -> Vec<Value> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    /// Run a tool; errors become text the model can react to.
    pub async fn call(&self, name: &str, args: Value, ctx: &ToolCtx) -> (bool, ToolOutput) {
        let Some(tool) = self.get(name) else {
            let known = self.names().join(", ");
            return (
                false,
                ToolOutput::new(format!("Unknown tool `{name}`. Available tools: {known}."), "unknown tool"),
            );
        };
        match tool.call(args, ctx).await {
            Ok(out) => (true, out),
            Err(e) => {
                let msg = format!("{e:#}");
                (
                    false,
                    ToolOutput::new(format!("Tool `{name}` failed: {msg}"), msg.chars().take(160).collect::<String>()),
                )
            }
        }
    }
}
