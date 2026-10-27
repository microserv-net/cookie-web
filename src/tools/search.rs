//! Web & news search with pluggable backends.
//!
//! Keyless: DuckDuckGo (HTML + lite endpoints) and Mojeek.
//! Optional: SearXNG (self-hosted, recommended), Brave, Tavily, Ollama web search.
//! Mode `fallback` tries backends in order; `fusion` queries all of them in
//! parallel and merges rankings with reciprocal-rank fusion.

use std::{collections::HashMap, sync::Arc};

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
        let html = resp.text().await?;
        let mut hits = parse_ddg_html(&html);
        if hits.is_empty() {
            // The HTML endpoint sometimes serves a bot challenge; the lite one often still works.
            let resp = web.client.post("https://lite.duckduckgo.com/lite/").form(&form).send().await?;
            hits = parse_ddg_lite(&resp.text().await?);
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

pub struct SearchEngine {
    pub backends: Vec<Box<dyn Backend>>,
    pub mode: SearchMode,
}

impl SearchEngine {
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
            backends.push(Box::new(Mojeek));
        }
        Self { backends, mode: cfg.search_mode.clone() }
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    pub async fn search(&self, web: &Web, q: &Query) -> anyhow::Result<Vec<Hit>> {
        if self.backends.is_empty() {
            bail!("no search backends configured");
        }
        let key = format!("{:?}|{:?}|{}|{}", q.kind, q.time_range, q.max, q.text.to_lowercase());
        if let Some(h) = web.search_cache.get(&key) {
            return Ok(h);
        }
        let hits = match self.mode {
            SearchMode::Fallback => {
                let mut errors = vec![];
                let mut found = vec![];
                for b in &self.backends {
                    match b.search(web, q).await {
                        Ok(h) if !h.is_empty() => {
                            found = h;
                            break;
                        }
                        Ok(_) => errors.push(format!("{}: no results", b.name())),
                        Err(e) => {
                            tracing::warn!("search backend {} failed: {e:#}", b.name());
                            errors.push(format!("{}: {e}", b.name()));
                        }
                    }
                }
                if found.is_empty() && !errors.is_empty() && errors.iter().all(|e| !e.ends_with("no results")) {
                    bail!("all search backends failed: {}", errors.join("; "));
                }
                found
            }
            SearchMode::Fusion => {
                let results = futures::future::join_all(self.backends.iter().map(|b| b.search(web, q))).await;
                let lists: Vec<Vec<Hit>> = results.into_iter().filter_map(Result::ok).collect();
                if lists.iter().all(Vec::is_empty) {
                    bail!("no search backend returned results");
                }
                fuse(lists, q.max)
            }
        };
        web.search_cache.put(key, hits.clone());
        Ok(hits)
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
        let q = Query { text: text.clone(), max, time_range: time_range_arg(&args), kind: self.kind };
        let hits = self.engine.search(&self.web, &q).await?;
        let kind = if self.kind == Kind::News { SourceKind::News } else { SourceKind::Search };
        if hits.is_empty() {
            return Ok(ToolOutput::new(format!("No results for \"{text}\". Try different keywords."), "no results"));
        }
        let engines: Vec<&str> =
            hits.iter().map(|h| h.engine.as_str()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        let body = render_hits(ctx, &format!("Results for \"{text}\":"), &hits, kind);
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
