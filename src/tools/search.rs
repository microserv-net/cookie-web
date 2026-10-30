//! Web & news search with pluggable backends.
//!
//! Keyless: DuckDuckGo (HTML + lite endpoints), Bing and Mojeek.
//! Optional: SearXNG (self-hosted, recommended), Brave, Tavily, Ollama web search.
//! Mode `fallback` tries backends in order until enough results are found; `fusion`
//! queries all of them in parallel and merges rankings with reciprocal-rank fusion.
//!
//! Reliability: every backend call is time-limited; a backend that keeps failing (rate limits,
//! captchas, outages) is put on a growing cooldown and skipped; if every backend fails, the whole
//! search is retried once; empty results are never cached.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use async_trait::async_trait;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolOutput, net::Web};
use crate::{
    config::{Config, SearchMode},
    sources::{SourceKind, domain_of, normalize_url},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engine: String,
    #[serde(default)]
    pub date: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Web,
    News,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub text: String,
    pub max: usize,
    /// day | week | month | year
    pub time_range: Option<String>,
    pub kind: Kind,
    /// Ask every backend and merge (instead of stopping at the first that answers): used when a first
    /// search came back off-topic, so one engine misreading the query can't sink the request.
    pub broad: bool,
}

#[async_trait]
pub trait Backend: Send + Sync {
    fn name(&self) -> &'static str;
    async fn search(&self, web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>>;
}

fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).unwrap()
}

// ---------------------------------------------------------------- DuckDuckGo

pub struct DuckDuckGo;

/// DuckDuckGo wraps result links as `//duckduckgo.com/l/?uddg=<encoded>`.
pub fn ddg_unwrap(href: &str) -> Option<String> {
    let full = if href.starts_with("//") { format!("https:{href}") } else { href.to_string() };
    let u = url::Url::parse(&full).ok()?;
    if u.host_str().map(|h| h.ends_with("duckduckgo.com")).unwrap_or(false) {
        if let Some((_, v)) = u.query_pairs().find(|(k, _)| k == "uddg") {
            return Some(v.into_owned());
        }
        return None; // internal DDG link (ads, settings)
    }
    Some(full)
}

pub fn parse_ddg_html(html: &str) -> Vec<Hit> {
    let doc = Html::parse_document(html);
    let mut out = vec![];
    for r in doc.select(&sel("div.result")) {
        let classes = r.value().attr("class").unwrap_or("");
        if classes.contains("result--ad") {
            continue;
        }
        let Some(a) = r.select(&sel("a.result__a")).next() else { continue };
        let Some(url) = a.value().attr("href").and_then(ddg_unwrap) else { continue };
        let snippet =
            r.select(&sel(".result__snippet")).next().map(|s| clean(&s.text().collect::<String>())).unwrap_or_default();
        out.push(Hit {
            title: clean(&a.text().collect::<String>()),
            url,
            snippet,
            engine: "duckduckgo".into(),
            date: None,
        });
    }
    out
}

pub fn parse_ddg_lite(html: &str) -> Vec<Hit> {
    let doc = Html::parse_document(html);
    let links: Vec<_> = doc.select(&sel("a.result-link")).collect();
    let snippets: Vec<String> =
        doc.select(&sel("td.result-snippet")).map(|s| clean(&s.text().collect::<String>())).collect();
    links
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            let url = a.value().attr("href").and_then(ddg_unwrap)?;
            Some(Hit {
                title: clean(&a.text().collect::<String>()),
                url,
                snippet: snippets.get(i).cloned().unwrap_or_default(),
                engine: "duckduckgo".into(),
                date: None,
            })
        })
        .collect()
}

fn ddg_df(t: &Option<String>) -> &'static str {
    match t.as_deref() {
        Some("day") => "d",
        Some("week") => "w",
        Some("month") => "m",
        Some("year") => "y",
        _ => "",
    }
}

#[async_trait]
impl Backend for DuckDuckGo {
    fn name(&self) -> &'static str {
        "duckduckgo"
    }
    async fn search(&self, web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let mut text = q.text.clone();
        let mut df = ddg_df(&q.time_range);
        if q.kind == Kind::News {
            text.push_str(" news");
            if df.is_empty() {
                df = "m";
            }
        }
        let form = [("q", text.as_str()), ("kl", "wt-wt"), ("df", df)];
        let resp = web
            .client
            .post("https://html.duckduckgo.com/html/")
            .header("Referer", "https://html.duckduckgo.com/")
            .form(&form)
            .send()
            .await?
            .error_for_status()?;
        // 202 + an "anomaly" page is DuckDuckGo's bot challenge.
        let mut limited = resp.status().as_u16() == 202;
        let html = resp.text().await?;
        limited |= html.contains("anomaly-modal") || html.contains("challenge-form");
        let mut hits = parse_ddg_html(&html);
        if hits.is_empty() {
            // The HTML endpoint sometimes serves a bot challenge; the lite one often still works.
            let resp = web.client.post("https://lite.duckduckgo.com/lite/").form(&form).send().await?;
            limited |= resp.status().as_u16() == 202;
            hits = parse_ddg_lite(&resp.text().await?);
        }
        if hits.is_empty() && limited {
            bail!("rate-limited by DuckDuckGo (bot challenge)");
        }
        hits.truncate(q.max);
        Ok(hits)
    }
}

// --------------------------------------------------------------------- Bing

pub struct Bing;

/// Decode unpadded URL-safe (or standard) base64.
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Bing wraps result links as `https://www.bing.com/ck/a?…&u=a1<base64url(target)>`.
pub fn bing_unwrap(href: &str) -> Option<String> {
    let u = url::Url::parse(href).ok()?;
    let host = u.host_str().unwrap_or("");
    if host == "bing.com" || host.ends_with(".bing.com") {
        let enc = u.query_pairs().find(|(k, _)| k == "u").map(|(_, v)| v.into_owned())?;
        let target = String::from_utf8(b64_decode(enc.strip_prefix("a1").unwrap_or(&enc))?).ok()?;
        return target.starts_with("http").then_some(target);
    }
    matches!(u.scheme(), "http" | "https").then(|| href.to_string())
}

pub fn parse_bing(html: &str) -> Vec<Hit> {
    let doc = Html::parse_document(html);
    let mut out = vec![];
    for li in doc.select(&sel("li.b_algo")) {
        let Some(a) = li.select(&sel("h2 a")).next() else { continue };
        let Some(url) = a.value().attr("href").and_then(bing_unwrap) else { continue };
        let date = li.select(&sel(".news_dt")).next().map(|d| clean(&d.text().collect::<String>()));
        let mut snippet = li
            .select(&sel(".b_caption p"))
            .next()
            .or_else(|| li.select(&sel("p")).next())
            .map(|p| clean(&p.text().collect::<String>()))
            .unwrap_or_default();
        if let Some(d) = date.as_deref().filter(|d| snippet.starts_with(*d)) {
            snippet = snippet[d.len()..].trim_start_matches(|c: char| c.is_whitespace() || c == '·').to_string();
        }
        out.push(Hit { title: clean(&a.text().collect::<String>()), url, snippet, engine: "bing".into(), date });
    }
    out
}

#[async_trait]
impl Backend for Bing {
    fn name(&self) -> &'static str {
        "bing"
    }
    async fn search(&self, web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let mut text = q.text.clone();
        let mut range = q.time_range.clone();
        if q.kind == Kind::News {
            text.push_str(" news");
            range.get_or_insert_with(|| "month".into());
        }
        let mut params = vec![("q", text), ("setlang", "en".to_string()), ("count", q.max.clamp(5, 20).to_string())];
        let filter = match range.as_deref() {
            Some("day") => Some("ex1:\"ez1\""),
            Some("week") => Some("ex1:\"ez2\""),
            Some("month") => Some("ex1:\"ez3\""),
            _ => None,
        };
        if let Some(f) = filter {
            params.push(("filters", f.to_string()));
        }
        let resp = web.client.get("https://www.bing.com/search").query(&params).send().await?.error_for_status()?;
        let html = resp.text().await?;
        let mut hits = parse_bing(&html);
        if hits.is_empty() && (html.contains("captcha") || html.contains("/challenge")) {
            bail!("rate-limited by Bing (captcha)");
        }
        hits.truncate(q.max);
        Ok(hits)
    }
}

// ------------------------------------------------------------------- Mojeek

pub struct Mojeek;

pub fn parse_mojeek(html: &str) -> Vec<Hit> {
    let doc = Html::parse_document(html);
    let mut out = vec![];
    for li in doc.select(&sel("ul.results-standard > li")) {
        let Some(a) = li.select(&sel("a.title")).next() else { continue };
        let Some(href) = a.value().attr("href") else { continue };
        let snippet = li.select(&sel("p.s")).next().map(|p| clean(&p.text().collect::<String>())).unwrap_or_default();
        out.push(Hit {
            title: clean(&a.text().collect::<String>()),
            url: href.to_string(),
            snippet,
            engine: "mojeek".into(),
            date: None,
        });
    }
    out
}

#[async_trait]
impl Backend for Mojeek {
    fn name(&self) -> &'static str {
        "mojeek"
    }
    async fn search(&self, web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let since = match q.time_range.as_deref() {
            Some("day") => "1",
            Some("week") => "7",
            Some("month") => "30",
            Some("year") => "365",
            _ => "",
        };
        let mut params = vec![("q", q.text.clone())];
        if !since.is_empty() {
            params.push(("since", since.into()));
        }
        let url = if q.kind == Kind::News {
            "https://www.mojeek.com/search?fmt=news"
        } else {
            "https://www.mojeek.com/search"
        };
        let html = web.client.get(url).query(&params).send().await?.error_for_status()?.text().await?;
        let mut hits = parse_mojeek(&html);
        hits.truncate(q.max);
        Ok(hits)
    }
}

// ------------------------------------------------------------------ SearXNG

pub struct Searxng(pub String);

#[async_trait]
impl Backend for Searxng {
    fn name(&self) -> &'static str {
        "searxng"
    }
    async fn search(&self, web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let mut params = vec![
            ("q", q.text.clone()),
            ("format", "json".to_string()),
            ("categories", if q.kind == Kind::News { "news" } else { "general" }.to_string()),
        ];
        if let Some(t) = &q.time_range {
            params.push(("time_range", t.clone()));
        }
        // SearXNG usually runs on the local network, so bypass the SSRF guard deliberately.
        let v: Value = web
            .client
            .get(format!("{}/search", self.0))
            .query(&params)
            .send()
            .await?
            .error_for_status()
            .context("SearXNG error (is the JSON format enabled in settings.yml?)")?
            .json()
            .await?;
        Ok(v["results"]
            .as_array()
            .into_iter()
            .flatten()
            .take(q.max)
            .filter_map(|r| {
                Some(Hit {
                    title: r["title"].as_str()?.to_string(),
                    url: r["url"].as_str()?.to_string(),
                    snippet: clean(r["content"].as_str().unwrap_or("")),
                    engine: "searxng".into(),
                    date: r["publishedDate"].as_str().map(String::from),
                })
            })
            .collect())
    }
}

// -------------------------------------------------------------------- Brave

pub struct Brave(pub String);

#[async_trait]
impl Backend for Brave {
    fn name(&self) -> &'static str {
        "brave"
    }
    async fn search(&self, web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let path = if q.kind == Kind::News { "news" } else { "web" };
        let mut params = vec![("q", q.text.clone()), ("count", q.max.min(20).to_string())];
        if let Some(f) = q.time_range.as_deref().and_then(|t| match t {
            "day" => Some("pd"),
            "week" => Some("pw"),
            "month" => Some("pm"),
            "year" => Some("py"),
            _ => None,
        }) {
            params.push(("freshness", f.into()));
        }
        let v: Value = web
            .client
            .get(format!("https://api.search.brave.com/res/v1/{path}/search"))
            .header("X-Subscription-Token", &self.0)
            .header("Accept", "application/json")
            .query(&params)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let arr = if q.kind == Kind::News { &v["results"] } else { &v["web"]["results"] };
        Ok(arr
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                Some(Hit {
                    title: r["title"].as_str()?.to_string(),
                    url: r["url"].as_str()?.to_string(),
                    snippet: clean(&strip_tags(r["description"].as_str().unwrap_or(""))),
                    engine: "brave".into(),
                    date: r["age"].as_str().or(r["page_age"].as_str()).map(String::from),
                })
            })
            .collect())
    }
}

fn strip_tags(s: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"<[^>]+>").unwrap());
    RE.replace_all(s, "").into_owned()
}

// ------------------------------------------------------------------- Tavily

pub struct Tavily(pub String);

#[async_trait]
impl Backend for Tavily {
    fn name(&self) -> &'static str {
        "tavily"
    }
    async fn search(&self, web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let mut body = json!({
            "query": q.text,
            "max_results": q.max.min(20),
            "topic": if q.kind == Kind::News { "news" } else { "general" },
        });
        if let Some(t) = &q.time_range {
            body["time_range"] = json!(t);
        }
        let v: Value = web
            .client
            .post("https://api.tavily.com/search")
            .bearer_auth(&self.0)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                Some(Hit {
                    title: r["title"].as_str()?.to_string(),
                    url: r["url"].as_str()?.to_string(),
                    snippet: clean(r["content"].as_str().unwrap_or("")).chars().take(400).collect(),
                    engine: "tavily".into(),
                    date: r["published_date"].as_str().map(String::from),
                })
            })
            .collect())
    }
}

// ------------------------------------------------------- Ollama web search

pub struct OllamaWeb(pub String);

#[async_trait]
impl Backend for OllamaWeb {
    fn name(&self) -> &'static str {
        "ollama"
    }
    async fn search(&self, web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let mut text = q.text.clone();
        if q.kind == Kind::News {
            text.push_str(" latest news");
        }
        let v: Value = web
            .client
            .post("https://ollama.com/api/web_search")
            .bearer_auth(&self.0)
            .json(&json!({ "query": text, "max_results": q.max.min(10) }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                Some(Hit {
                    title: r["title"].as_str()?.to_string(),
                    url: r["url"].as_str()?.to_string(),
                    snippet: clean(r["content"].as_str().unwrap_or("")).chars().take(400).collect(),
                    engine: "ollama".into(),
                    date: None,
                })
            })
            .collect())
    }
}

// ------------------------------------------------------------------- Engine

/// One backend call may take at most this long.
const BACKEND_TIMEOUT: Duration = Duration::from_secs(15);
/// In fallback mode, keep asking further backends until this many results are in hand.
const ENOUGH_RESULTS: usize = 4;
/// Longest cooldown for a failing backend.
const MAX_COOLDOWN: Duration = Duration::from_secs(600);

#[derive(Default)]
struct Health {
    ok: u64,
    failures: u64,
    consecutive: u32,
    until: Option<Instant>,
    last_error: Option<String>,
}

/// Public view of a backend's health (`/api/health`, `doctor`).
#[derive(Clone, Debug, Serialize)]
pub struct BackendStatus {
    pub name: &'static str,
    pub ok: u64,
    pub failures: u64,
    /// Seconds until the backend is tried again (0 = available).
    pub cooldown_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

pub struct SearchEngine {
    pub backends: Vec<Box<dyn Backend>>,
    pub mode: SearchMode,
    health: Mutex<HashMap<&'static str, Health>>,
    /// Pause before the second pass when every backend failed.
    pub retry_pause: Duration,
}

impl SearchEngine {
    pub fn new(backends: Vec<Box<dyn Backend>>, mode: SearchMode) -> Self {
        Self { backends, mode, health: Mutex::new(HashMap::new()), retry_pause: Duration::from_millis(1500) }
    }

    pub fn from_config(cfg: &Config) -> Self {
        let mut backends: Vec<Box<dyn Backend>> = vec![];
        if let Some(u) = &cfg.searxng_url {
            backends.push(Box::new(Searxng(u.clone())));
        }
        if let Some(k) = &cfg.brave_api_key {
            backends.push(Box::new(Brave(k.clone())));
        }
        if let Some(k) = &cfg.tavily_api_key {
            backends.push(Box::new(Tavily(k.clone())));
        }
        if let Some(k) = &cfg.ollama_api_key {
            backends.push(Box::new(OllamaWeb(k.clone())));
        }
        if !cfg.disable_duckduckgo {
            backends.push(Box::new(DuckDuckGo));
            backends.push(Box::new(Bing));
            backends.push(Box::new(Mojeek));
        }
        Self::new(backends, cfg.search_mode.clone())
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    pub fn status(&self) -> Vec<BackendStatus> {
        let h = self.health.lock().unwrap();
        self.backends
            .iter()
            .map(|b| {
                let s = h.get(b.name());
                BackendStatus {
                    name: b.name(),
                    ok: s.map_or(0, |s| s.ok),
                    failures: s.map_or(0, |s| s.failures),
                    cooldown_secs: s
                        .and_then(|s| s.until)
                        .map(|u| u.saturating_duration_since(Instant::now()).as_secs())
                        .unwrap_or(0),
                    last_error: s.and_then(|s| s.last_error.clone()),
                }
            })
            .collect()
    }

    fn cooling(&self, name: &str) -> bool {
        let h = self.health.lock().unwrap();
        h.get(name).and_then(|s| s.until).is_some_and(|u| u > Instant::now())
    }

    fn record(&self, name: &'static str, result: Result<(), &str>) {
        let mut h = self.health.lock().unwrap();
        let s = h.entry(name).or_default();
        match result {
            Ok(()) => {
                s.ok += 1;
                s.consecutive = 0;
                s.until = None;
            }
            Err(e) => {
                s.failures += 1;
                s.consecutive += 1;
                s.last_error = Some(e.chars().take(200).collect());
                if s.consecutive >= 2 {
                    let secs = 30u64.saturating_mul(1 << (s.consecutive - 2).min(5));
                    s.until = Some(Instant::now() + Duration::from_secs(secs).min(MAX_COOLDOWN));
                }
            }
        }
    }

    /// Backends to try, in order: healthy ones first; cooling-down ones only if nothing else is left.
    fn order(&self, include_cooling: bool) -> Vec<&dyn Backend> {
        let (warm, cold): (Vec<&dyn Backend>, Vec<&dyn Backend>) =
            self.backends.iter().map(|b| b.as_ref()).partition(|b| !self.cooling(b.name()));
        if warm.is_empty() || include_cooling { warm.into_iter().chain(cold).collect() } else { warm }
    }

    /// One time-limited backend call, with health accounting.
    async fn ask(&self, b: &dyn Backend, web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let res = match tokio::time::timeout(BACKEND_TIMEOUT, b.search(web, q)).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("timed out after {}s", BACKEND_TIMEOUT.as_secs())),
        };
        match &res {
            Ok(_) => self.record(b.name(), Ok(())),
            Err(e) => {
                tracing::warn!("search backend {} failed: {e:#}", b.name());
                self.record(b.name(), Err(&format!("{e:#}")));
            }
        }
        res
    }

    pub async fn search(&self, web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>> {
        if self.backends.is_empty() {
            bail!("no search backends configured");
        }
        let key = format!("{:?}|{:?}|{}|{}|{}", q.kind, q.time_range, q.max, q.broad, q.text.to_lowercase());
        if let Some(h) = web.search_cache.get(&key) {
            return Ok(h);
        }
        let mut errors = vec![];
        let mut hits = vec![];
        for pass in 0..2 {
            if pass == 1 {
                tokio::time::sleep(self.retry_pause).await;
            }
            errors.clear();
            let (lists, had_answer) = match self.mode {
                SearchMode::Fallback if !q.broad => self.fallback(web, q, pass == 1, &mut errors).await,
                _ => self.fusion(web, q, pass == 1, &mut errors).await,
            };
            hits = fuse(lists, q.max);
            // Retry only when every backend *failed*; "no results" is an answer.
            if !hits.is_empty() || had_answer {
                break;
            }
        }
        if hits.is_empty() && !errors.is_empty() && errors.iter().all(|e| !e.ends_with("no results")) {
            bail!("all search backends failed: {}", errors.join("; "));
        }
        if !hits.is_empty() {
            web.search_cache.put(key, hits.clone());
        }
        Ok(hits)
    }

    async fn fallback(&self, web: &Web, q: &Query, all: bool, errors: &mut Vec<String>) -> (Vec<Vec<Hit>>, bool) {
        let mut lists = vec![];
        let mut got = 0;
        let mut answered = false;
        for b in self.order(all) {
            match self.ask(b, web, q).await {
                Ok(h) if !h.is_empty() => {
                    answered = true;
                    got += h.len();
                    lists.push(h);
                    if got >= ENOUGH_RESULTS.min(q.max) {
                        break;
                    }
                }
                Ok(_) => {
                    answered = true;
                    errors.push(format!("{}: no results", b.name()));
                }
                Err(e) => errors.push(format!("{}: {e}", b.name())),
            }
        }
        (lists, answered)
    }

    async fn fusion(&self, web: &Web, q: &Query, all: bool, errors: &mut Vec<String>) -> (Vec<Vec<Hit>>, bool) {
        let order = self.order(all);
        let results = futures::future::join_all(order.iter().map(|b| self.ask(*b, web, q))).await;
        let mut lists = vec![];
        let mut answered = false;
        for (b, r) in order.iter().zip(results) {
            match r {
                Ok(h) => {
                    answered = true;
                    if h.is_empty() {
                        errors.push(format!("{}: no results", b.name()));
                    } else {
                        lists.push(h);
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", b.name())),
            }
        }
        (lists, answered)
    }
}

/// Reciprocal-rank fusion (k = 60) over several ranked lists, de-duplicated by URL.
pub fn fuse(lists: Vec<Vec<Hit>>, max: usize) -> Vec<Hit> {
    let mut scores: HashMap<String, (f64, Hit)> = HashMap::new();
    for list in lists {
        for (rank, hit) in list.into_iter().enumerate() {
            let key = normalize_url(&hit.url);
            let s = 1.0 / (60.0 + rank as f64 + 1.0);
            scores
                .entry(key)
                .and_modify(|(score, h)| {
                    *score += s;
                    if h.snippet.len() < hit.snippet.len() {
                        h.snippet = hit.snippet.clone();
                    }
                    if !h.engine.contains(&hit.engine) {
                        h.engine = format!("{}+{}", h.engine, hit.engine);
                    }
                })
                .or_insert((s, hit));
        }
    }
    let mut v: Vec<(f64, Hit)> = scores.into_values().collect();
    v.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    v.into_iter().take(max).map(|(_, h)| h).collect()
}

// -------------------------------------------------------------------- Tools

pub struct WebSearchTool {
    pub web: Arc<Web>,
    pub engine: Arc<SearchEngine>,
    pub kind: Kind,
}

fn time_range_arg(args: &Value) -> Option<String> {
    args["time_range"]
        .as_str()
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| matches!(s.as_str(), "day" | "week" | "month" | "year"))
}

/// A looser version of a query that matched nothing: no quotes, brackets or search operators.
/// `None` if loosening would not change it.
pub fn relax_query(q: &str) -> Option<String> {
    let words: Vec<String> = q
        .split_whitespace()
        .filter(|w| {
            let l = w.to_ascii_lowercase();
            !(l.starts_with("site:")
                || l.starts_with("intitle:")
                || l.starts_with("inurl:")
                || l.starts_with("filetype:"))
                && !(w.starts_with('-') && w.len() > 1)
        })
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '.' && c != '+' && c != '#'))
        .map(|w| w.trim_matches('\'').to_string())
        .filter(|w| !w.is_empty())
        .collect();
    let relaxed = words.join(" ");
    let site = q.split_whitespace().find(|w| w.to_ascii_lowercase().starts_with("site:"));
    let relaxed = match site {
        Some(s) if !relaxed.is_empty() => format!("{relaxed} {s}"),
        _ => relaxed,
    };
    (!relaxed.is_empty() && relaxed != q.trim()).then_some(relaxed)
}

/// Format hits for the model and register them as sources.
pub fn render_hits(ctx: &ToolCtx, header: &str, hits: &[Hit], kind: SourceKind) -> String {
    let mut out = format!("{header}\n");
    for h in hits {
        let n = ctx.add_source(&h.title, &h.url, &h.snippet, kind.clone(), h.date.clone());
        let date = h.date.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
        out.push_str(&format!("[{n}] {} — {}{date}\n    URL: {}\n", h.title, domain_of(&h.url), h.url));
        if !h.snippet.is_empty() {
            out.push_str(&format!("    {}\n", h.snippet.chars().take(320).collect::<String>()));
        }
    }
    out
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Web => "web_search",
            Kind::News => "news_search",
        }
    }
    fn description(&self) -> &'static str {
        match self.kind {
            Kind::Web => {
                "Search the web. Returns numbered results [n] with title, URL and snippet. Use specific keywords; run several searches with different phrasings for broad questions."
            }
            Kind::News => {
                "Search recent news articles. Use for current events, launches, announcements, anything time-sensitive. Results include dates when available."
            }
        }
    }
    fn timeout(&self) -> Duration {
        Duration::from_secs(100)
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search keywords" },
                "time_range": { "type": "string", "enum": ["day", "week", "month", "year", "any"], "description": "Only results from this period" },
                "site": { "type": "string", "description": "Restrict to a domain, e.g. github.com" },
                "max_results": { "type": "integer", "description": "1-10, default 8" }
            },
            "required": ["query"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        let mut text = args["query"].as_str().unwrap_or("").trim().to_string();
        if text.is_empty() {
            bail!("`query` is required");
        }
        if let Some(site) = args["site"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            text = format!(
                "{text} site:{}",
                site.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/')
            );
        }
        let max = args["max_results"].as_u64().map(|n| n.clamp(1, 10) as usize).unwrap_or(ctx.results_per_search);
        let broad = args["broad"].as_bool().unwrap_or(false);
        let mut q = Query { text: text.clone(), max, time_range: time_range_arg(&args), kind: self.kind, broad };
        let mut hits = self.engine.search(&self.web, &q).await?;
        let mut header = format!("Results for \"{text}\":");
        // Exact phrases and odd punctuation often match nothing: retry once with plain keywords,
        // and if a time filter emptied the results, once without it.
        if hits.is_empty()
            && let Some(relaxed) = relax_query(&text)
        {
            q.text = relaxed.clone();
            hits = self.engine.search(&self.web, &q).await.unwrap_or_default();
            header = format!("No results for \"{text}\"; results for \"{relaxed}\" instead:");
        }
        if hits.is_empty() && q.time_range.is_some() {
            q.time_range = None;
            hits = self.engine.search(&self.web, &q).await.unwrap_or_default();
            header = format!("No recent results; results from any time for \"{}\":", q.text);
        }
        let kind = if self.kind == Kind::News { SourceKind::News } else { SourceKind::Search };
        if hits.is_empty() {
            return Ok(ToolOutput::new(
                format!("No results for \"{text}\". Try different, simpler or more general keywords."),
                "no results",
            )
            .with_data(json!({ "hits": [] })));
        }
        let engines: Vec<&str> =
            hits.iter().map(|h| h.engine.as_str()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        let body = render_hits(ctx, &header, &hits, kind);
        Ok(ToolOutput::new(body, format!("{} results via {}", hits.len(), engines.join(", ")))
            .with_data(json!({ "hits": hits })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_ddg_redirects() {
        assert_eq!(
            ddg_unwrap("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=xyz").as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(ddg_unwrap("https://example.com/").as_deref(), Some("https://example.com/"));
        assert_eq!(ddg_unwrap("https://duckduckgo.com/y.js?ad_domain=x"), None);
    }

    #[test]
    fn parses_ddg_html() {
        let html = r##"<div class="result results_links web-result"><div class="links_main">
          <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FVision_Quest">Vision quest - Wikipedia</a></h2>
          <a class="result__snippet" href="#">A <b>vision quest</b> is a rite of passage.</a></div></div>
          <div class="result result--ad"><a class="result__a" href="https://ads.example">Ad</a></div>"##;
        let hits = parse_ddg_html(html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://en.wikipedia.org/wiki/Vision_Quest");
        assert_eq!(hits[0].snippet, "A vision quest is a rite of passage.");
    }

    #[test]
    fn parses_mojeek() {
        let html = r#"<ul class="results-standard"><li><a class="ob" href="https://a.com">a.com</a>
          <h2><a class="title" href="https://a.com/x">Title X</a></h2><p class="s">Snippet x</p></li></ul>"#;
        let hits = parse_mojeek(html);
        assert_eq!(hits[0].title, "Title X");
        assert_eq!(hits[0].snippet, "Snippet x");
    }

    #[test]
    fn parses_bing_and_unwraps_redirects() {
        let html = r#"<ol id="b_results"><li class="b_algo"><div class="b_tpcn"><a class="tilk" href="https://www.bing.com/ck/a?!&amp;&amp;p=x&amp;u=a1aHR0cHM6Ly93d3cubGVhcm5yZWxpZ2lvbnMuY29tL3NhbXVlbC1sYXN0LW9mLXRoZS1qdWRnZXMtNzAxMTYx&amp;ntb=1">site</a></div>
          <h2><a href="https://www.bing.com/ck/a?!&amp;&amp;p=x&amp;u=a1aHR0cHM6Ly93d3cubGVhcm5yZWxpZ2lvbnMuY29tL3NhbXVlbC1sYXN0LW9mLXRoZS1qdWRnZXMtNzAxMTYx&amp;ntb=1">Who Was <strong>Samuel</strong>? - Learn Religions</a></h2>
          <div class="b_caption"><p class="b_lineclamp2"><span class="news_dt">Jun 18, 2024</span>&nbsp;&#0183;&#32;In the Bible, Samuel was a prophet.</p></div></li>
          <li class="b_algo"><h2><a href="https://example.org/direct">Direct link</a></h2><div class="b_caption"><p>Plain snippet.</p></div></li>
          <li class="b_algo"><h2><a href="https://www.bing.com/images/search?q=x">Bing internal</a></h2></li></ol>"#;
        let hits = parse_bing(html);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].url, "https://www.learnreligions.com/samuel-last-of-the-judges-701161");
        assert_eq!(hits[0].title, "Who Was Samuel? - Learn Religions");
        assert_eq!(hits[0].date.as_deref(), Some("Jun 18, 2024"));
        assert_eq!(hits[0].snippet, "In the Bible, Samuel was a prophet.");
        assert_eq!(hits[1].url, "https://example.org/direct");
        assert_eq!(hits[1].snippet, "Plain snippet.");
    }

    #[test]
    fn relaxes_queries() {
        assert_eq!(relax_query("\"stares motherfuckingly\" samuel").as_deref(), Some("stares motherfuckingly samuel"));
        assert_eq!(relax_query("rust async -tokio site:docs.rs").as_deref(), Some("rust async site:docs.rs"));
        assert_eq!(relax_query("plain words"), None);
    }

    // ---- engine behaviour with scripted backends

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Scripted {
        name: &'static str,
        calls: Arc<AtomicUsize>,
        /// per-call results: Some(n) = n hits, None = error; the last entry repeats
        script: Vec<Option<usize>>,
    }

    #[async_trait]
    impl Backend for Scripted {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn search(&self, _web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>> {
            let i = self.calls.fetch_add(1, Ordering::SeqCst);
            match self.script.get(i).or(self.script.last()).copied().flatten() {
                None => bail!("HTTP 429 Too Many Requests"),
                Some(n) => Ok((0..n)
                    .map(|k| Hit {
                        title: format!("{} {k}", self.name),
                        url: format!("https://{}.example/{}/{k}", self.name, q.text.replace(' ', "-")),
                        snippet: String::new(),
                        engine: self.name.into(),
                        date: None,
                    })
                    .collect()),
            }
        }
    }

    fn scripted(name: &'static str, script: Vec<Option<usize>>) -> (Box<dyn Backend>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (Box::new(Scripted { name, calls: calls.clone(), script }), calls)
    }

    fn web() -> Web {
        Web::new(Arc::new(Config::for_tests("reports".into()))).unwrap()
    }

    fn q(text: &str) -> Query {
        Query { text: text.into(), max: 8, time_range: None, kind: Kind::Web, broad: false }
    }

    #[tokio::test]
    async fn fallback_tops_up_from_the_next_backend() {
        let (a, _) = scripted("a", vec![Some(2)]);
        let (b, b_calls) = scripted("b", vec![Some(3)]);
        let (c, c_calls) = scripted("c", vec![Some(3)]);
        let e = SearchEngine::new(vec![a, b, c], SearchMode::Fallback);
        let hits = e.search(&web(), &q("x")).await.unwrap();
        assert_eq!(hits.len(), 5, "2 from a + 3 from b");
        assert_eq!(b_calls.load(Ordering::SeqCst), 1);
        assert_eq!(c_calls.load(Ordering::SeqCst), 0, "enough results already");
    }

    #[tokio::test]
    async fn failing_backend_cools_down_and_is_skipped() {
        let (a, a_calls) = scripted("a", vec![None]);
        let (b, _) = scripted("b", vec![Some(5)]);
        let e = SearchEngine::new(vec![a, b], SearchMode::Fallback);
        let w = web();
        for i in 0..4 {
            e.search(&w, &q(&format!("query {i}"))).await.unwrap();
        }
        assert_eq!(a_calls.load(Ordering::SeqCst), 2, "skipped after two failures in a row");
        let st = e.status();
        assert!(st[0].cooldown_secs > 0 && st[0].failures == 2, "{st:?}");
        assert_eq!(st[1].ok, 4);
    }

    #[tokio::test]
    async fn retries_once_when_every_backend_fails() {
        let (a, a_calls) = scripted("a", vec![None, Some(4)]);
        let mut e = SearchEngine::new(vec![a], SearchMode::Fallback);
        e.retry_pause = std::time::Duration::from_millis(1);
        let hits = e.search(&web(), &q("x")).await.unwrap();
        assert_eq!(hits.len(), 4);
        assert_eq!(a_calls.load(Ordering::SeqCst), 2);

        let (b, _) = scripted("b", vec![None]);
        let mut e = SearchEngine::new(vec![b], SearchMode::Fusion);
        e.retry_pause = std::time::Duration::from_millis(1);
        let err = e.search(&web(), &q("y")).await.unwrap_err();
        assert!(format!("{err}").contains("all search backends failed"));
    }

    #[tokio::test]
    async fn empty_results_are_not_cached() {
        let (a, a_calls) = scripted("a", vec![Some(0), Some(3)]);
        let e = SearchEngine::new(vec![a], SearchMode::Fallback);
        let w = web();
        assert!(e.search(&w, &q("x")).await.unwrap().is_empty());
        assert_eq!(e.search(&w, &q("x")).await.unwrap().len(), 3, "asked again, not served an empty cache entry");
        assert_eq!(e.search(&w, &q("x")).await.unwrap().len(), 3);
        assert_eq!(a_calls.load(Ordering::SeqCst), 2, "non-empty result was cached");
    }

    #[tokio::test]
    async fn broad_queries_ask_every_backend() {
        let (a, _) = scripted("a", vec![Some(5)]);
        let (b, b_calls) = scripted("b", vec![Some(5)]);
        let e = SearchEngine::new(vec![a, b], SearchMode::Fallback);
        let w = web();
        e.search(&w, &q("x")).await.unwrap();
        assert_eq!(b_calls.load(Ordering::SeqCst), 0, "fallback stops at the first good answer");
        let hits = e.search(&w, &Query { broad: true, ..q("x") }).await.unwrap();
        assert_eq!(b_calls.load(Ordering::SeqCst), 1, "broad asks everyone");
        assert!(hits.iter().any(|h| h.engine == "a") && hits.iter().any(|h| h.engine == "b"));
    }

    #[test]
    fn fusion_merges_and_ranks() {
        let h = |u: &str, e: &str| Hit {
            title: u.into(),
            url: u.into(),
            snippet: String::new(),
            engine: e.into(),
            date: None,
        };
        let a = vec![h("https://x.com/1", "a"), h("https://x.com/2", "a")];
        let b = vec![h("https://x.com/2", "b"), h("https://x.com/3", "b")];
        let f = fuse(vec![a, b], 10);
        assert_eq!(f[0].url, "https://x.com/2");
        assert_eq!(f[0].engine, "a+b");
        assert_eq!(f.len(), 3);
    }
}
