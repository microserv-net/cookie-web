//! Shared HTTP layer: one client, SSRF guard, size limits, TTL caches.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use futures::StreamExt;

use super::extract::{Page, html_to_page, pdf_to_page};
use crate::config::Config;

/// Tiny TTL cache with a size cap (oldest entries evicted first).
pub struct TtlCache<T: Clone> {
    ttl: Duration,
    cap: usize,
    map: Mutex<HashMap<String, (Instant, T)>>,
}

impl<T: Clone> TtlCache<T> {
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self { ttl, cap, map: Mutex::new(HashMap::new()) }
    }
    pub fn get(&self, k: &str) -> Option<T> {
        let m = self.map.lock().unwrap();
        m.get(k).filter(|(t, _)| t.elapsed() < self.ttl).map(|(_, v)| v.clone())
    }
    pub fn put(&self, k: String, v: T) {
        let mut m = self.map.lock().unwrap();
        if m.len() >= self.cap {
            let ttl = self.ttl;
            m.retain(|_, (t, _)| t.elapsed() < ttl);
            if m.len() >= self.cap
                && let Some(oldest) = m.iter().min_by_key(|(_, (t, _))| *t).map(|(k, _)| k.clone())
            {
                m.remove(&oldest);
            }
        }
        m.insert(k, (Instant::now(), v));
    }
}

fn ip_is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) // CGNAT
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link local
                || v6.to_ipv4_mapped().map(|v4| ip_is_private(&IpAddr::V4(v4))).unwrap_or(false)
        }
    }
}

fn host_looks_private(host: &str) -> bool {
    let h = host.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") || h.ends_with(".local") || h.ends_with(".internal") {
        return true;
    }
    h.parse::<IpAddr>().map(|ip| ip_is_private(&ip)).unwrap_or(false)
}

pub struct Web {
    pub client: reqwest::Client,
    pub cfg: Arc<Config>,
    pages: TtlCache<Arc<Page>>,
    pub search_cache: TtlCache<Vec<super::search::Hit>>,
}

impl Web {
    pub fn new(cfg: Arc<Config>) -> anyhow::Result<Self> {
        let allow_private = cfg.allow_private_urls;
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 8 {
                return attempt.error("too many redirects");
            }
            let bad = attempt.url().host_str().map(host_looks_private).unwrap_or(true);
            if bad && !allow_private {
                attempt.error("redirect to a private address blocked")
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .timeout(cfg.fetch_timeout)
            .connect_timeout(Duration::from_secs(10))
            .redirect(policy)
            .build()?;
        Ok(Self {
            client,
            cfg,
            pages: TtlCache::new(Duration::from_secs(30 * 60), 512),
            search_cache: TtlCache::new(Duration::from_secs(15 * 60), 512),
        })
    }

    /// Reject non-http(s) URLs and (unless allowed) anything resolving to a private network.
    pub async fn check_url(&self, raw: &str) -> anyhow::Result<url::Url> {
        let u = url::Url::parse(raw.trim()).with_context(|| format!("invalid URL: {raw}"))?;
        if !matches!(u.scheme(), "http" | "https") {
            bail!("only http(s) URLs are allowed");
        }
        let host = u.host_str().ok_or_else(|| anyhow!("URL has no host"))?.to_string();
        if self.cfg.allow_private_urls {
            return Ok(u);
        }
        if host_looks_private(&host) {
            bail!("refusing to fetch a private/local address ({host})");
        }
        let port = u.port_or_known_default().unwrap_or(443);
        if let Ok(addrs) = tokio::net::lookup_host((host.as_str(), port)).await {
            for a in addrs {
                if ip_is_private(&a.ip()) {
                    bail!("refusing to fetch {host}: resolves to a private address");
                }
            }
        }
        Ok(u)
    }

    /// GET a URL with a body-size cap. Returns (final_url, content_type, bytes).
    pub async fn get_bytes(&self, raw: &str) -> anyhow::Result<(String, String, Vec<u8>)> {
        let u = self.check_url(raw).await?;
        let resp = self
            .client
            .get(u)
            .header("Accept", "text/html,application/xhtml+xml,application/pdf;q=0.9,text/plain;q=0.8,*/*;q=0.5")
            .header("Accept-Language", "en-US,en;q=0.8")
            .send()
            .await
            .with_context(|| format!("request failed: {raw}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("HTTP {status} for {raw}");
        }
        let final_url = resp.url().to_string();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let max = self.cfg.max_page_bytes;
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len() + chunk.len() > max {
                body.extend_from_slice(&chunk[..max.saturating_sub(body.len())]);
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok((final_url, ctype, body))
    }

    /// Fetch and extract a page (cached).
    pub async fn page(&self, raw: &str) -> anyhow::Result<Arc<Page>> {
        let key = crate::sources::normalize_url(raw);
        if let Some(p) = self.pages.get(&key) {
            return Ok(p);
        }
        let (final_url, ctype, body) = self.get_bytes(raw).await?;
        let is_pdf = ctype.contains("pdf") || body.starts_with(b"%PDF");
        let page = if is_pdf {
            let u = final_url.clone();
            tokio::task::spawn_blocking(move || pdf_to_page(&body, &u))
                .await?
                .ok_or_else(|| anyhow!("could not extract text from PDF"))?
        } else if ctype.contains("html") || ctype.is_empty() || body.trim_ascii_start().starts_with(b"<") {
            let html = String::from_utf8_lossy(&body).into_owned();
            let u = final_url.clone();
            tokio::task::spawn_blocking(move || html_to_page(&html, &u, &ctype)).await?
        } else if ctype.starts_with("text/") || ctype.contains("json") || ctype.contains("xml") {
            Page {
                url: final_url.clone(),
                title: final_url.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&final_url).to_string(),
                markdown: String::from_utf8_lossy(&body).into_owned(),
                published: None,
                byline: None,
                site_name: None,
                content_type: ctype,
                links: vec![],
            }
        } else {
            bail!("unsupported content type '{ctype}' at {raw}");
        };
        if page.markdown.trim().is_empty() {
            bail!("no readable content at {raw} (the page may need JavaScript)");
        }
        let page = Arc::new(page);
        self.pages.put(key, page.clone());
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_hosts() {
        assert!(host_looks_private("localhost"));
        assert!(host_looks_private("127.0.0.1"));
        assert!(host_looks_private("10.1.2.3"));
        assert!(host_looks_private("[::1]"));
        assert!(host_looks_private("192.168.1.5"));
        assert!(!host_looks_private("example.com"));
        assert!(!host_looks_private("8.8.8.8"));
    }

    #[test]
    fn cache_evicts() {
        let c = TtlCache::new(Duration::from_secs(60), 2);
        c.put("a".into(), 1);
        c.put("b".into(), 2);
        c.put("c".into(), 3);
        assert_eq!(c.get("c"), Some(3));
        assert_eq!(c.map.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn blocks_private_urls() {
        let web = Web::new(Arc::new(Config::for_tests("reports".into()))).unwrap();
        assert!(web.check_url("http://127.0.0.1:11434/api/tags").await.is_err());
        assert!(web.check_url("file:///etc/passwd").await.is_err());
    }
}
