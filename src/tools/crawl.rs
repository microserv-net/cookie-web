//! `crawl_site`: breadth-first crawl of one site (respecting robots.txt),
//! returning the most relevant excerpt of each page.

use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};

use anyhow::bail;
use async_trait::async_trait;
use futures::{StreamExt, stream};
use serde_json::{Value, json};

use super::{
    Tool, ToolCtx, ToolOutput,
    extract::{focus_excerpt, terms},
    net::Web,
};
use crate::sources::{SourceKind, domain_of};

const SKIP_EXT: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico", ".css", ".js", ".zip", ".gz", ".tar", ".exe", ".dmg",
    ".mp4", ".mp3", ".woff", ".woff2", ".ttf", ".xml", ".rss", ".json",
];

/// Disallow rules for `User-agent: *` from a robots.txt body.
pub fn parse_robots(body: &str) -> Vec<String> {
    let mut rules = vec![];
    let mut applies = false;
    let mut in_agents = false;
    for line in body.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "user-agent" => {
                if !in_agents {
                    applies = false;
                }
                in_agents = true;
                if v == "*" || v.to_ascii_lowercase().contains("cookie-web") {
                    applies = true;
                }
            }
            "disallow" => {
                in_agents = false;
                if applies && !v.is_empty() {
                    rules.push(v.to_string());
                }
            }
            _ => in_agents = false,
        }
    }
    rules
}

fn allowed(path: &str, rules: &[String]) -> bool {
    !rules.iter().any(|r| {
        let r = r.trim_end_matches('*');
        if let Some(exact) = r.strip_suffix('$') { path == exact } else { path.starts_with(r) }
    })
}

pub struct CrawlSite {
    pub web: Arc<Web>,
}

#[async_trait]
impl Tool for CrawlSite {
    fn name(&self) -> &'static str {
        "crawl_site"
    }
    fn description(&self) -> &'static str {
        "Crawl a website starting from a URL (same domain only, robots.txt respected) and return the most \
         relevant excerpt of each page. Use for documentation sites, blogs or when an answer is spread across \
         several pages of one site. Costly: prefer fetch_page for a single page."
    }
    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(150)
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Start URL" },
                "focus": { "type": "string", "description": "What you are looking for; guides which links to follow" },
                "max_pages": { "type": "integer", "description": "1-15, default 6" }
            },
            "required": ["url"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        let start = args["url"].as_str().unwrap_or("").trim().to_string();
        if start.is_empty() {
            bail!("`url` is required");
        }
        let start_url = self.web.check_url(&start).await?;
        let host = domain_of(start_url.as_str());
        let focus = args["focus"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let focus_terms = focus.as_deref().map(terms).unwrap_or_default();
        let max_pages = args["max_pages"].as_u64().map(|n| n.clamp(1, 15) as usize).unwrap_or(6);
        // a crawl costs a few page reads of budget
        if !ctx.take_fetch() {
            return Ok(ToolOutput::new(
                "Page-reading budget exhausted. Answer with what you have.",
                "budget exhausted",
            ));
        }
        let second = ctx.take_fetch();

        let robots_url = format!("{}://{}/robots.txt", start_url.scheme(), start_url.host_str().unwrap_or(""));
        let rules = match self.web.get_bytes(&robots_url).await {
            Ok((_, _, b)) => parse_robots(&String::from_utf8_lossy(&b)),
            Err(_) => vec![],
        };

        let per_page = (ctx.page_chars / max_pages.max(1)).clamp(500, 2000);
        let mut queue: VecDeque<(String, usize)> = VecDeque::from([(start_url.to_string(), 0)]);
        let mut seen: HashSet<String> = HashSet::from([crate::sources::normalize_url(start_url.as_str())]);
        let mut pages = vec![];

        while !queue.is_empty() && pages.len() < max_pages {
            let batch: Vec<(String, usize)> = (0..4).filter_map(|_| queue.pop_front()).collect();
            let results: Vec<_> = stream::iter(batch)
                .map(|(u, d)| async move { (self.web.page(&u).await, d) })
                .buffer_unordered(4)
                .collect()
                .await;
            for (res, depth) in results {
                let Ok(page) = res else { continue };
                if pages.len() >= max_pages {
                    break;
                }
                if depth < 2 {
                    // follow on-site links, most relevant (by anchor/URL text) first
                    let mut cand: Vec<(usize, String)> = page
                        .links
                        .iter()
                        .filter(|l| domain_of(&l.url) == host)
                        .filter(|l| {
                            let low = l.url.to_lowercase();
                            !SKIP_EXT.iter().any(|e| low.ends_with(e))
                        })
                        .filter(|l| url::Url::parse(&l.url).map(|u| allowed(u.path(), &rules)).unwrap_or(false))
                        .map(|l| {
                            let hay = format!("{} {}", l.text, l.url).to_lowercase();
                            (focus_terms.iter().filter(|t| hay.contains(t.as_str())).count(), l.url.clone())
                        })
                        .collect();
                    cand.sort_by_key(|c| std::cmp::Reverse(c.0));
                    for (_, u) in cand.into_iter().take(25) {
                        if seen.insert(crate::sources::normalize_url(&u)) {
                            queue.push_back((u, depth + 1));
                        }
                    }
                }
                pages.push(page);
            }
        }
        if pages.is_empty() {
            ctx.refund_fetch();
            if second {
                ctx.refund_fetch();
            }
            bail!("could not read any page from {start}");
        }
        let mut text = format!("Crawled {} pages on {host}:\n", pages.len());
        for p in &pages {
            let n = ctx.add_source(&p.title, &p.url, "", SourceKind::Page, p.published.clone());
            text.push_str(&format!(
                "\n---\n[{n}] {}\nURL: {}\n{}\n",
                p.title,
                p.url,
                focus_excerpt(&super::extract::for_model(&p.markdown, ctx.keep_urls), focus.as_deref(), per_page)
            ));
        }
        Ok(ToolOutput::new(text, format!("crawled {} pages on {host}", pages.len())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robots_rules() {
        let body =
            "User-agent: Googlebot\nDisallow: /g\n\nUser-agent: *\nDisallow: /private\nDisallow: /tmp$\nAllow: /";
        let r = parse_robots(body);
        assert_eq!(r, vec!["/private".to_string(), "/tmp$".to_string()]);
        assert!(!allowed("/private/x", &r));
        assert!(allowed("/g/x", &r));
        assert!(!allowed("/tmp", &r));
        assert!(allowed("/tmp/x", &r));
    }
}
