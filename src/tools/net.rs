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
    /// Wayback Machine endpoints (overridable for tests).
    pub archive_base: String,
    pub archive_web: String,
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
            archive_base: "https://archive.org".into(),
            archive_web: "https://web.archive.org".into(),
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
    ///
    /// Transient failures (connection errors, time-outs, 429/5xx) are retried once after a short
    /// back-off; a 401/403 is retried once with plain browser headers (some sites reject unusual clients).
    pub async fn get_bytes(&self, raw: &str) -> anyhow::Result<(String, String, Vec<u8>)> {
        let u = self.check_url(raw).await?;
        let mut last_err = None;
        for attempt in 0..2 {
            match self.get_once(&u, attempt > 0).await {
                Ok(ok) => return Ok(ok),
                Err(FetchError { err, retry_after }) => {
                    let Some(wait) = retry_after.filter(|_| attempt == 0) else { return Err(err) };
                    tracing::debug!("retrying {raw} in {}ms: {err:#}", wait.as_millis());
                    tokio::time::sleep(wait).await;
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("request failed: {raw}")))
    }

    async fn get_once(&self, u: &url::Url, browser_like: bool) -> Result<(String, String, Vec<u8>), FetchError> {
        let raw = u.as_str();
        let mut req = self
            .client
            .get(u.clone())
            .header("Accept", "text/html,application/xhtml+xml,application/pdf;q=0.9,text/plain;q=0.8,*/*;q=0.5")
            .header("Accept-Language", "en-US,en;q=0.8");
        if browser_like {
            req = req
                .header(reqwest::header::USER_AGENT, BROWSER_UA)
                .header("Sec-Fetch-Dest", "document")
                .header("Sec-Fetch-Mode", "navigate")
                .header("Sec-Fetch-Site", "none")
                .header("Upgrade-Insecure-Requests", "1");
        }
        let resp = req.send().await.map_err(|e| {
            let transient = e.is_timeout() || e.is_connect() || e.is_request();
            FetchError::new(anyhow::Error::new(e).context(format!("request failed: {raw}")), transient)
        })?;
        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|s| Duration::from_secs(s.min(3)));
            let transient = status.as_u16() == 429
                || (status.is_server_error() && status.as_u16() != 501)
                || status.as_u16() == 401
                || status.as_u16() == 403;
            let mut e = FetchError::new(anyhow!("HTTP {status} for {raw}"), transient);
            if transient && let Some(w) = retry_after {
                e.retry_after = Some(w);
            }
            return Err(e);
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
            let chunk = chunk.map_err(|e| FetchError::new(anyhow!("reading {raw} failed: {e}"), true))?;
            if body.len() + chunk.len() > max {
                body.extend_from_slice(&chunk[..max.saturating_sub(body.len())]);
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok((final_url, ctype, body))
    }

    /// Fetch and extract a page (cached). If the live page can't be read and the archive fallback is on,
    /// the most recent Wayback Machine copy is read instead (its URL is the page URL then).
    pub async fn page(&self, raw: &str) -> anyhow::Result<Arc<Page>> {
        let key = crate::sources::normalize_url(raw);
        if let Some(p) = self.pages.get(&key) {
            return Ok(p);
        }
        let live = match readable_mirror(raw) {
            // Some sites serve a JavaScript shell; their lightweight mirror has the same content as HTML.
            Some(m) => match self.read_page(&m).await {
                Ok(p) => Ok(p),
                Err(_) => self.read_page(raw).await,
            },
            None => self.read_page(raw).await,
        };
        let page = match live {
            Ok(p) => p,
            Err(e) if self.cfg.archive_fallback && worth_archive(&e) => match self.archived(raw).await {
                Some(p) => {
                    tracing::info!("read archived copy of {raw} ({e:#})");
                    p
                }
                None => return Err(e),
            },
            Err(e) => return Err(e),
        };
        let page = Arc::new(page);
        self.pages.put(key, page.clone());
        Ok(page)
    }

    async fn read_page(&self, raw: &str) -> anyhow::Result<Page> {
        let (final_url, ctype, body) = self.get_bytes(raw).await?;
        let page = bytes_to_page(final_url, ctype, body).await?;
        if page.markdown.trim().is_empty() {
            bail!("no readable content at {raw} (the page may need JavaScript)");
        }
        if let Some(why) = blocked_page(&page) {
            bail!("{raw} returned {why} instead of its content");
        }
        Ok(page)
    }

    /// The newest Wayback Machine snapshot of `raw`, read as a page.
    async fn archived(&self, raw: &str) -> Option<Page> {
        let api = format!("{}/wayback/available", self.archive_base);
        let v: serde_json::Value = self
            .client
            .get(&api)
            .query(&[("url", raw)])
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let snap = &v["archived_snapshots"]["closest"];
        if snap["available"].as_bool() != Some(true) {
            return None;
        }
        let ts = snap["timestamp"].as_str()?;
        // `id_` asks for the original bytes, without the Wayback toolbar.
        let url = format!("{}/web/{ts}id_/{raw}", self.archive_web);
        let (_, ctype, body) = self.get_bytes(&url).await.ok()?;
        let mut page = bytes_to_page(url, ctype, body).await.ok()?;
        if page.markdown.trim().is_empty() {
            return None;
        }
        // Cite the human-viewable snapshot, and say it is an archived copy.
        page.url = format!("{}/web/{ts}/{raw}", self.archive_web);
        let date = format!("{}-{}-{}", ts.get(..4)?, ts.get(4..6)?, ts.get(6..8)?);
        page.title = format!("{} (archived {date})", if page.title.is_empty() { raw } else { &page.title });
        Some(page)
    }
}

/// Recognise bot walls, consent screens and empty shells that would otherwise count as a "read" page.
pub fn blocked_page(page: &Page) -> Option<&'static str> {
    let words = page.markdown.split_whitespace().count();
    if words >= 300 {
        return None;
    }
    let head = format!("{} {}", page.title, page.markdown.chars().take(800).collect::<String>()).to_lowercase();
    const WALLS: &[&str] = &[
        "just a moment",
        "attention required",
        "access denied",
        "verify you are human",
        "are you a robot",
        "are you human",
        "checking your browser",
        "enable javascript and cookies",
        "please enable javascript",
        "request blocked",
        "unusual traffic",
        "captcha",
        "403 forbidden",
        "before you continue",
    ];
    if WALLS.iter().any(|w| head.contains(w)) {
        return Some("a bot check or consent page");
    }
    if words < 25 && page.content_type.contains("html") {
        return Some("an almost empty page");
    }
    None
}

/// A server-rendered mirror of a page whose normal version needs JavaScript (Reddit → old.reddit.com).
pub fn readable_mirror(raw: &str) -> Option<String> {
    let mut u = url::Url::parse(raw).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    if matches!(host.as_str(), "reddit.com" | "www.reddit.com" | "new.reddit.com" | "np.reddit.com") {
        u.set_host(Some("old.reddit.com")).ok()?;
        return Some(u.to_string());
    }
    None
}

/// A browser-like User-Agent for the second attempt after a 401/403.
const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

struct FetchError {
    err: anyhow::Error,
    /// `Some` = worth one more try after this wait.
    retry_after: Option<Duration>,
}

impl FetchError {
    fn new(err: anyhow::Error, transient: bool) -> Self {
        Self { err, retry_after: transient.then_some(Duration::from_millis(700)) }
    }
}

/// Is this fetch failure one the Wayback Machine might rescue (dead link, blocked, down)?
fn worth_archive(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}");
    !m.contains("refusing")
        && !m.contains("only http")
        && !m.contains("invalid URL")
        && !m.contains("unsupported content")
}

/// Decode an HTML body: charset from the Content-Type header, else a `<meta charset>`, else UTF-8.
pub fn decode_html(body: &[u8], ctype: &str) -> String {
    fn label(s: &str) -> Option<&'static encoding_rs::Encoding> {
        let i = s.find("charset=")? + 8;
        let rest = s[i..].trim_start_matches(['"', '\'', ' ']);
        let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).unwrap_or(rest.len());
        encoding_rs::Encoding::for_label(&rest.as_bytes()[..end])
    }
    let head = String::from_utf8_lossy(&body[..body.len().min(4096)]).to_ascii_lowercase();
    let enc = encoding_rs::Encoding::for_bom(body)
        .map(|(e, _)| e)
        .or_else(|| label(ctype))
        .or_else(|| label(&head))
        .unwrap_or(encoding_rs::UTF_8);
    // A page that claims Latin-1 but is valid UTF-8 is almost always UTF-8.
    if enc != encoding_rs::UTF_8 && std::str::from_utf8(body).is_ok() && enc.name() == "windows-1252" {
        return String::from_utf8_lossy(body).into_owned();
    }
    enc.decode(body).0.into_owned()
}

/// Turn fetched bytes into a [`Page`] (CPU-bound parsing runs off the async threads).
async fn bytes_to_page(final_url: String, ctype: String, body: Vec<u8>) -> anyhow::Result<Page> {
    let is_pdf = ctype.contains("pdf") || body.starts_with(b"%PDF");
    if is_pdf {
        let u = final_url.clone();
        return tokio::task::spawn_blocking(move || pdf_to_page(&body, &u))
            .await?
            .ok_or_else(|| anyhow!("could not extract text from PDF"));
    }
    if ctype.contains("html") || ctype.is_empty() || body.trim_ascii_start().starts_with(b"<") {
        let u = final_url.clone();
        return Ok(tokio::task::spawn_blocking(move || {
            let html = decode_html(&body, &ctype);
            html_to_page(&html, &u, &ctype)
        })
        .await?);
    }
    if ctype.starts_with("text/") || ctype.contains("json") || ctype.contains("xml") {
        return Ok(Page {
            url: final_url.clone(),
            title: final_url.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&final_url).to_string(),
            markdown: decode_html(&body, &ctype),
            published: None,
            byline: None,
            site_name: None,
            content_type: ctype,
            links: vec![],
        });
    }
    bail!("unsupported content type '{ctype}' at {final_url}")
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
    fn spots_bot_walls() {
        let page = |title: &str, md: &str| Page {
            url: "https://x.com".into(),
            title: title.into(),
            markdown: md.into(),
            published: None,
            byline: None,
            site_name: None,
            content_type: "text/html".into(),
            links: vec![],
        };
        assert!(blocked_page(&page("Just a moment...", "Checking your browser before accessing x.com.")).is_some());
        assert!(blocked_page(&page("Welcome to Reddit", "Skip to main content")).is_some());
        let article = "word ".repeat(400);
        assert!(blocked_page(&page("How CAPTCHAs work", &article)).is_none());
        assert!(
            blocked_page(&page("Short note", &"a real short page with enough words to count ".repeat(5))).is_none()
        );
    }

    #[test]
    fn reddit_reads_from_old_reddit() {
        assert_eq!(
            readable_mirror("https://www.reddit.com/r/rust/comments/abc/x/").as_deref(),
            Some("https://old.reddit.com/r/rust/comments/abc/x/")
        );
        assert_eq!(readable_mirror("https://example.com/"), None);
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
