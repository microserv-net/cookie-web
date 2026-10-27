//! `wikipedia`: fast, reliable summaries for names, terms and concepts.

use std::sync::Arc;

use anyhow::bail;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolOutput, net::Web};
use crate::sources::SourceKind;

/// Wikimedia asks API clients for a descriptive User-Agent.
const WIKI_UA: &str =
    concat!("cookie-web/", env!("CARGO_PKG_VERSION"), " (https://github.com/microserv-net/cookie-web)");

pub struct Wikipedia {
    pub web: Arc<Web>,
}

#[async_trait]
impl Tool for Wikipedia {
    fn name(&self) -> &'static str {
        "wikipedia"
    }
    fn description(&self) -> &'static str {
        "Look up a topic on Wikipedia: returns the best-matching article summary plus other candidate articles \
         (useful to disambiguate names and terms)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "topic": { "type": "string" },
                "lang": { "type": "string", "description": "Wikipedia language code, default en" }
            },
            "required": ["topic"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        let topic = args["topic"].as_str().or(args["query"].as_str()).unwrap_or("").trim();
        if topic.is_empty() {
            bail!("`topic` is required");
        }
        let lang: String = args["lang"]
            .as_str()
            .unwrap_or("en")
            .chars()
            .filter(|c| c.is_ascii_alphabetic() || *c == '-')
            .take(12)
            .collect();
        let lang = if lang.is_empty() { "en".to_string() } else { lang };
        let base = format!("https://{lang}.wikipedia.org");
        let found: Value = self
            .web
            .client
            .get(format!("{base}/w/rest.php/v1/search/page"))
            .header(reqwest::header::USER_AGENT, WIKI_UA)
            .query(&[("q", topic), ("limit", "5")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let pages = found["pages"].as_array().cloned().unwrap_or_default();
        let Some(top) = pages.first() else {
            return Ok(ToolOutput::new(format!("No Wikipedia article found for \"{topic}\"."), "no article"));
        };
        let key = top["key"].as_str().unwrap_or("");
        let summary: Value = self
            .web
            .client
            .get(format!("{base}/api/rest_v1/page/summary/{}", urlencode(key)))
            .header(reqwest::header::USER_AGENT, WIKI_UA)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let title = summary["title"].as_str().unwrap_or(key);
        let url = summary["content_urls"]["desktop"]["page"]
            .as_str()
            .map(String::from)
            .unwrap_or(format!("{base}/wiki/{key}"));
        let extract = summary["extract"].as_str().unwrap_or("");
        let desc = summary["description"].as_str().unwrap_or("");
        let n = ctx.add_source(&format!("{title} — Wikipedia"), &url, extract, SourceKind::Wiki, None);
        let mut text = format!("[{n}] {title} — Wikipedia\nURL: {url}\n");
        if !desc.is_empty() {
            text.push_str(&format!("Description: {desc}\n"));
        }
        if summary["type"].as_str() == Some("disambiguation") {
            text.push_str("(This is a disambiguation page: the term has several meanings.)\n");
        }
        text.push_str(&format!("\n{extract}\n"));
        let others: Vec<String> = pages
            .iter()
            .skip(1)
            .filter_map(|p| {
                let t = p["title"].as_str()?;
                let d = p["description"].as_str().unwrap_or("");
                Some(format!(
                    "- {t}{} — {base}/wiki/{}",
                    if d.is_empty() { String::new() } else { format!(" ({d})") },
                    p["key"].as_str()?
                ))
            })
            .collect();
        if !others.is_empty() {
            text.push_str(&format!("\nOther matching articles:\n{}\n", others.join("\n")));
        }
        Ok(ToolOutput::new(text, format!("[{n}] {title}")))
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20")
}
