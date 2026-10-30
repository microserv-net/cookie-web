//! The tool belt. Each tool is described with a JSON schema (sent to the model
//! for native tool calling) and returns text for the model plus a short
//! summary for the UI. Tools register every URL they touch as a citable source.

pub mod crawl;
pub mod extract;
pub mod fetch;
pub mod net;
pub mod search;
pub mod wiki;

use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicIsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::FutureExt;
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
    /// Show page links with their URLs (requests that want links, e.g. downloads); otherwise
    /// pages are shown to the model with link text only.
    pub keep_urls: bool,
}

impl ToolCtx {
    pub fn new(events: Events, results_per_search: usize, page_chars: usize, max_fetches: usize) -> Self {
        Self {
            sources: Arc::new(Mutex::new(SourceRegistry::new())),
            events,
            results_per_search,
            page_chars,
            fetches_left: Arc::new(AtomicIsize::new(max_fetches as isize)),
            keep_urls: false,
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

    /// Use an existing source registry (a follow-up continues its parent's numbering).
    pub fn with_sources(self, reg: SourceRegistry) -> Self {
        *self.sources.lock().unwrap() = reg;
        self
    }

    /// True if this job already read the page at `url` in full.
    pub fn already_read(&self, url: &str) -> bool {
        let key = crate::sources::normalize_url(url);
        self.sources.lock().unwrap().all().iter().any(|s| s.read && crate::sources::normalize_url(&s.url) == key)
    }

    /// Consume one unit of the page-reading budget. Returns false if exhausted.
    pub fn take_fetch(&self) -> bool {
        let mut n = self.fetches_left.load(Ordering::SeqCst);
        while n > 0 {
            match self.fetches_left.compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(now) => n = now,
            }
        }
        false
    }

    pub fn fetches_left(&self) -> isize {
        self.fetches_left.load(Ordering::SeqCst).max(0)
    }

    /// Give back one unit of page budget (the read failed, so it shouldn't count).
    pub fn refund_fetch(&self) {
        self.fetches_left.fetch_add(1, Ordering::SeqCst);
    }

    pub fn add_fetch_budget(&self, n: usize) {
        self.fetches_left.fetch_add(n as isize, Ordering::SeqCst);
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;
    async fn call(&self, args: Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput>;

    /// Hard time limit for one call; a stuck call fails instead of stalling the job.
    fn timeout(&self) -> Duration {
        Duration::from_secs(75)
    }

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
    /// The shared search engine (for health reporting), when the standard tools are used.
    pub search: Option<Arc<search::SearchEngine>>,
    /// The shared HTTP layer (page cache, reading sources for follow-ups), when the standard tools are used.
    pub web: Option<Arc<net::Web>>,
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
        r.add(wiki::Wikipedia { web: web.clone() });
        r.search = Some(engine);
        r.web = Some(web);
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

    /// Run a tool; errors, time-outs and even panics become text the model can react to,
    /// so one misbehaving tool or web page never takes the whole job down.
    pub async fn call(&self, name: &str, args: Value, ctx: &ToolCtx) -> (bool, ToolOutput) {
        let Some(tool) = self.get(name) else {
            let known = self.names().join(", ");
            return (
                false,
                ToolOutput::new(format!("Unknown tool `{name}`. Available tools: {known}."), "unknown tool"),
            );
        };
        if !args.is_object() && !args.is_null() {
            return (
                false,
                ToolOutput::new(format!("Tool `{name}` needs a JSON object of arguments."), "bad arguments"),
            );
        }
        let limit = tool.timeout();
        let fut = AssertUnwindSafe(tool.call(args, ctx)).catch_unwind();
        let failed = |msg: String| {
            (false, ToolOutput::new(format!("Tool `{name}` failed: {msg}"), msg.chars().take(160).collect::<String>()))
        };
        match tokio::time::timeout(limit, fut).await {
            Ok(Ok(Ok(out))) => (true, out),
            Ok(Ok(Err(e))) => failed(format!("{e:#}")),
            Ok(Err(panic)) => {
                let what = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic".into());
                tracing::error!("tool {name} panicked: {what}");
                failed(format!("internal error ({what})"))
            }
            Err(_) => failed(format!("timed out after {}s", limit.as_secs())),
        }
    }
}
