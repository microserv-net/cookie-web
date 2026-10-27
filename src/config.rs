//! Runtime configuration, read from environment variables (and an optional `.env` file).
//!
//! Every setting has a sensible default so `cookie-web serve` works out of the box
//! with a local Ollama and DuckDuckGo as the search backend.

use std::{env, path::PathBuf, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchMode {
    /// Try backends in order; the first one that returns results wins.
    Fallback,
    /// Query every configured backend in parallel and merge with reciprocal-rank fusion.
    Fusion,
}

/// How the model participates in gathering information (quick/standard requests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentMode {
    /// The model drives extra tool calls (native tool calling, or the JSON protocol as fallback).
    Auto,
    /// No model-driven tool rounds: code searches and reads the top results, the model only
    /// triages and writes. Fastest and most robust on tiny models / slow CPUs.
    Pipeline,
}

/// The default model: the smallest model that still does reliable tool calling and cited
/// synthesis. Non-thinking (no wasted tokens), 2.5 GB at Q4_K_M. See docs/HARDWARE.md.
pub const DEFAULT_MODEL: &str = "qwen3:4b-instruct";

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,

    pub ollama_host: String,
    pub model: String,
    /// Optional smaller/faster model used for triage, planning and note-taking.
    pub fast_model: Option<String>,
    pub think: bool,
    pub num_ctx: u32,
    /// CPU threads Ollama may use for this model (`None` = Ollama's default: all physical cores).
    pub num_thread: Option<u32>,
    pub temperature: f32,
    /// How long Ollama keeps the model loaded. `-1` = forever (default).
    pub keep_alive: String,
    pub agent_mode: AgentMode,
    /// Load the model into memory when the server starts.
    pub warmup: bool,

    pub search_mode: SearchMode,
    pub searxng_url: Option<String>,
    pub brave_api_key: Option<String>,
    pub tavily_api_key: Option<String>,
    pub ollama_api_key: Option<String>,
    /// Drop the keyless scraping backends (DuckDuckGo, Bing, Mojeek).
    pub disable_duckduckgo: bool,

    pub reports_dir: PathBuf,
    pub api_key: Option<String>,
    pub max_concurrent: usize,
    pub allow_private_urls: bool,
    pub user_agent: String,
    pub fetch_timeout: Duration,
    pub max_page_bytes: usize,
    pub cors_origins: Vec<String>,

    /// Hard limit for one research job; it fails with a clear error instead of hanging.
    pub job_timeout: Duration,
    /// Multiplies the per-depth gathering time budgets (raise on slow CPUs, lower on GPUs).
    pub time_scale: f64,
    /// When a page can't be fetched (dead link, 403, timeout), try its Wayback Machine copy.
    pub archive_fallback: bool,
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    var(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn flag(name: &str, default: bool) -> bool {
    match var(name).map(|v| v.to_ascii_lowercase()) {
        Some(v) => matches!(v.as_str(), "1" | "true" | "yes" | "on"),
        None => default,
    }
}

impl Config {
    pub fn from_env() -> Self {
        let _ = dotenvy::dotenv();
        let search_mode = match var("COOKIE_SEARCH_MODE").as_deref() {
            Some("fusion") => SearchMode::Fusion,
            _ => SearchMode::Fallback,
        };
        Self {
            // All interfaces by default: cookie-web runs on its own box and is used over the network.
            host: var("COOKIE_HOST").unwrap_or_else(|| "0.0.0.0".into()),
            port: parse("COOKIE_PORT", 8765),
            ollama_host: var("OLLAMA_HOST")
                .map(|h| if h.starts_with("http") { h } else { format!("http://{h}") })
                .unwrap_or_else(|| "http://127.0.0.1:11434".into())
                .trim_end_matches('/')
                .to_string(),
            model: var("COOKIE_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into()),
            fast_model: var("COOKIE_FAST_MODEL"),
            think: flag("COOKIE_THINK", false),
            num_ctx: parse("COOKIE_NUM_CTX", 8192u32).clamp(4096, 262_144),
            num_thread: var("COOKIE_NUM_THREAD").and_then(|v| v.parse::<u32>().ok()).filter(|n| *n > 0),
            temperature: parse("COOKIE_TEMPERATURE", 0.3),
            keep_alive: var("COOKIE_KEEP_ALIVE").unwrap_or_else(|| "-1".into()),
            agent_mode: match var("COOKIE_AGENT_MODE").map(|v| v.to_ascii_lowercase()).as_deref() {
                Some("pipeline") => AgentMode::Pipeline,
                _ => AgentMode::Auto,
            },
            warmup: flag("COOKIE_WARMUP", true),
            search_mode,
            searxng_url: var("SEARXNG_URL").map(|u| u.trim_end_matches('/').to_string()),
            brave_api_key: var("BRAVE_API_KEY"),
            tavily_api_key: var("TAVILY_API_KEY"),
            ollama_api_key: var("OLLAMA_API_KEY"),
            disable_duckduckgo: flag("COOKIE_DISABLE_SCRAPERS", false) || flag("COOKIE_DISABLE_DUCKDUCKGO", false),
            reports_dir: PathBuf::from(var("COOKIE_REPORTS_DIR").unwrap_or_else(|| "reports".into())),
            api_key: var("COOKIE_API_KEY"),
            // One model instance serves one request at a time on small boxes; queue the rest.
            max_concurrent: parse("COOKIE_MAX_CONCURRENT", 1usize).max(1),
            allow_private_urls: flag("COOKIE_ALLOW_PRIVATE_URLS", false),
            user_agent: var("COOKIE_USER_AGENT").unwrap_or_else(|| {
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/130.0 Safari/537.36 cookie-web/0.1"
                    .into()
            }),
            fetch_timeout: Duration::from_secs(parse("COOKIE_FETCH_TIMEOUT_SECS", 20u64)),
            max_page_bytes: parse("COOKIE_MAX_PAGE_BYTES", 6 * 1024 * 1024usize),
            cors_origins: var("COOKIE_CORS_ORIGINS")
                .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
                .unwrap_or_else(|| vec!["*".into()]),
            job_timeout: Duration::from_secs(parse("COOKIE_JOB_TIMEOUT_SECS", 1800u64).max(30)),
            time_scale: parse("COOKIE_TIME_SCALE", 1.0f64).clamp(0.1, 20.0),
            archive_fallback: flag("COOKIE_ARCHIVE_FALLBACK", true),
        }
    }

    /// The model used for cheap structured steps (triage, planning, notes).
    pub fn fast_model(&self) -> &str {
        self.fast_model.as_deref().unwrap_or(&self.model)
    }

    /// `keep_alive` as Ollama expects it: a number of seconds (negative = forever) or a duration string.
    pub fn keep_alive_json(&self) -> serde_json::Value {
        match self.keep_alive.parse::<i64>() {
            Ok(n) => serde_json::json!(n),
            Err(_) => serde_json::json!(self.keep_alive),
        }
    }

    /// A config suitable for tests: no network backends besides what tests inject.
    pub fn for_tests(reports_dir: PathBuf) -> Self {
        let mut c = Self::from_env_defaults();
        c.reports_dir = reports_dir;
        c
    }

    fn from_env_defaults() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 0,
            ollama_host: "http://127.0.0.1:11434".into(),
            model: "test-model".into(),
            fast_model: None,
            think: false,
            num_ctx: 8192,
            num_thread: None,
            temperature: 0.2,
            keep_alive: "5m".into(),
            agent_mode: AgentMode::Auto,
            warmup: false,
            search_mode: SearchMode::Fallback,
            searxng_url: None,
            brave_api_key: None,
            tavily_api_key: None,
            ollama_api_key: None,
            disable_duckduckgo: true,
            reports_dir: PathBuf::from("reports"),
            api_key: None,
            max_concurrent: 2,
            allow_private_urls: false,
            user_agent: "cookie-web-test".into(),
            fetch_timeout: Duration::from_secs(5),
            max_page_bytes: 1024 * 1024,
            cors_origins: vec!["*".into()],
            job_timeout: Duration::from_secs(120),
            time_scale: 1.0,
            archive_fallback: false,
        }
    }
}
