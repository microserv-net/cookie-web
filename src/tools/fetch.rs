//! `fetch_page` (read a URL as clean Markdown) and `extract_links`.

use std::sync::Arc;

use anyhow::bail;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    Tool, ToolCtx, ToolOutput,
    extract::{focus_excerpt, for_model},
    net::Web,
};
use crate::sources::SourceKind;

pub struct FetchPage {
    pub web: Arc<Web>,
}

#[async_trait]
impl Tool for FetchPage {
    fn name(&self) -> &'static str {
        "fetch_page"
    }
    fn description(&self) -> &'static str {
        "Open a URL and read its main content as clean Markdown (works for articles, docs, PDFs). \
         Give `focus` (what you are looking for) to get the most relevant passages of long pages."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Absolute http(s) URL" },
                "focus": { "type": "string", "description": "What to look for on the page (keywords or a question)" }
            },
            "required": ["url"]
        })
    }
    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(90)
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        let url = args["url"].as_str().unwrap_or("").trim();
        if url.is_empty() {
            bail!("`url` is required");
        }
        // Re-reading a page this job already read costs nothing (it is cached).
        let already = ctx.already_read(url);
        if !already && !ctx.take_fetch() {
            return Ok(ToolOutput::new(
                "Page-reading budget exhausted. Answer with the information you already have.",
                "budget exhausted",
            ));
        }
        let focus = args["focus"].as_str().map(str::trim).filter(|s| !s.is_empty());
        let page = match self.web.page(url).await {
            Ok(p) => p,
            Err(e) => {
                // A failed read must not eat the budget, or one dead link can leave a quick answer with no page.
                if !already {
                    ctx.refund_fetch();
                }
                return Err(e);
            }
        };
        let n = ctx.add_source(&page.title, &page.url, "", SourceKind::Page, page.published.clone());
        let body = focus_excerpt(&for_model(&page.markdown, ctx.keep_urls), focus, ctx.page_chars);
        let mut meta = vec![format!("URL: {}", page.url)];
        if let Some(d) = &page.published {
            meta.push(format!("Published: {d}"));
        }
        if let Some(b) = &page.byline {
            meta.push(format!("By: {b}"));
        }
        if !page.images.is_empty() {
            meta.push(format!("{} images", page.images.len()));
        }
        let text = format!("[{n}] {}\n{}\n\n{}", page.title, meta.join(" | "), body);
        let words = page.markdown.split_whitespace().count();
        Ok(ToolOutput::new(text, format!("read [{n}] {} ({words} words)", page.title))
            .with_data(json!({ "n": n, "url": page.url, "title": page.title, "words": words })))
    }
}

pub struct ExtractLinks {
    pub web: Arc<Web>,
}

#[async_trait]
impl Tool for ExtractLinks {
    fn name(&self) -> &'static str {
        "extract_links"
    }
    fn description(&self) -> &'static str {
        "List the links on a page (URL + anchor text). Use to find downloads, sub-pages, docs sections or \
         official resources. Optional `filter` keeps only links whose URL or text contains it."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "filter": { "type": "string", "description": "Case-insensitive substring, e.g. '.pdf' or 'download'" },
                "same_site": { "type": "boolean", "description": "Only links on the same domain" }
            },
            "required": ["url"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        let url = args["url"].as_str().unwrap_or("").trim();
        if url.is_empty() {
            bail!("`url` is required");
        }
        let page = self.web.page(url).await?;
        let n = ctx.add_source(&page.title, &page.url, "", SourceKind::Page, None);
        let filter = args["filter"].as_str().map(|s| s.to_lowercase()).filter(|s| !s.is_empty());
        let same = args["same_site"].as_bool().unwrap_or(false);
        let host = crate::sources::domain_of(&page.url);
        let links: Vec<_> = page
            .links
            .iter()
            .filter(|l| !same || crate::sources::domain_of(&l.url) == host)
            .filter(|l| {
                filter
                    .as_ref()
                    .map(|f| l.url.to_lowercase().contains(f) || l.text.to_lowercase().contains(f))
                    .unwrap_or(true)
            })
            .take(60)
            .collect();
        let mut text = format!("Links on [{n}] {} ({} shown of {}):\n", page.title, links.len(), page.links.len());
        for l in &links {
            text.push_str(&format!("- {} — {}\n", if l.text.is_empty() { "(no text)" } else { &l.text }, l.url));
        }
        Ok(ToolOutput::new(text, format!("{} links on {}", links.len(), host)).with_data(json!({ "links": links })))
    }
}
