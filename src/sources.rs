//! Source registry: every URL a tool touches gets a stable citation number `[n]`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Search,
    News,
    Page,
    Wiki,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Source {
    /// Citation number, 1-based.
    pub n: usize,
    pub title: String,
    pub url: String,
    pub domain: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snippet: String,
    pub kind: SourceKind,
    /// True once the full page was read (not just a search snippet).
    #[serde(default)]
    pub read: bool,
    /// Publication date if known (free-form, as reported by the source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// True if the final answer cites this source.
    #[serde(default)]
    pub cited: bool,
}

#[derive(Debug, Default, Clone)]
pub struct SourceRegistry {
    items: Vec<Source>,
}

fn is_tracking_param(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.starts_with("utm_")
        || matches!(
            k.as_str(),
            "fbclid"
                | "gclid"
                | "dclid"
                | "msclkid"
                | "msockid"
                | "ref"
                | "ref_src"
                | "mc_cid"
                | "mc_eid"
                | "igshid"
                | "yclid"
        )
}

/// Remove tracking parameters (utm_*, fbclid, Bing's msockid, …) from a URL, keeping everything else.
pub fn strip_tracking(raw: &str) -> String {
    let Ok(mut u) = url::Url::parse(raw.trim()) else { return raw.trim().to_string() };
    if !u.query_pairs().any(|(k, _)| is_tracking_param(&k)) {
        return raw.trim().to_string();
    }
    let kept: Vec<(String, String)> =
        u.query_pairs().filter(|(k, _)| !is_tracking_param(k)).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    u.to_string()
}

/// Normalise a URL for de-duplication: drop fragment, tracking params, trailing slash, `www.`.
pub fn normalize_url(raw: &str) -> String {
    let Ok(mut u) = url::Url::parse(raw.trim()) else {
        return raw.trim().to_string();
    };
    u.set_fragment(None);
    let kept: Vec<(String, String)> =
        u.query_pairs().filter(|(k, _)| !is_tracking_param(k)).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    let host = u.host_str().unwrap_or("").trim_start_matches("www.").to_ascii_lowercase();
    let path = u.path().trim_end_matches('/');
    let q = u.query().map(|q| format!("?{q}")).unwrap_or_default();
    format!("{host}{path}{q}")
}

pub fn domain_of(raw: &str) -> String {
    url::Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
        .unwrap_or_default()
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a source (or update an existing one) and return its citation number.
    pub fn add(&mut self, title: &str, url: &str, snippet: &str, kind: SourceKind) -> usize {
        let clean = strip_tracking(url);
        let url = clean.as_str();
        let key = normalize_url(url);
        if let Some(s) = self.items.iter_mut().find(|s| normalize_url(&s.url) == key) {
            if s.title.is_empty() || (kind == SourceKind::Page && !title.is_empty()) {
                s.title = title.to_string();
            }
            if s.snippet.is_empty() {
                s.snippet = snippet.to_string();
            }
            if kind == SourceKind::Page {
                s.read = true;
            }
            return s.n;
        }
        let n = self.items.len() + 1;
        self.items.push(Source {
            n,
            title: if title.trim().is_empty() { domain_of(url) } else { title.trim().to_string() },
            url: url.to_string(),
            domain: domain_of(url),
            snippet: snippet.chars().take(400).collect(),
            read: kind == SourceKind::Page,
            kind,
            date: None,
            cited: false,
        });
        n
    }

    pub fn set_date(&mut self, n: usize, date: Option<String>) {
        if let (Some(s), Some(d)) = (self.items.get_mut(n.wrapping_sub(1)), date) {
            s.date = Some(d);
        }
    }

    pub fn get(&self, n: usize) -> Option<&Source> {
        self.items.get(n.wrapping_sub(1))
    }

    pub fn mark_cited(&mut self, n: usize) {
        if let Some(s) = self.items.get_mut(n.wrapping_sub(1)) {
            s.cited = true;
        }
    }

    pub fn all(&self) -> &[Source] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Renumber sources so citations read 1, 2, 3… in the order they first appear in the answer.
    /// `first` lists old numbers to put first (in order); the rest keep their relative order.
    /// Returns the old → new mapping.
    pub fn renumber(&mut self, first: &[usize]) -> std::collections::HashMap<usize, usize> {
        let mut order: Vec<usize> = vec![];
        for &n in first {
            if n >= 1 && n <= self.items.len() && !order.contains(&n) {
                order.push(n);
            }
        }
        for s in &self.items {
            if !order.contains(&s.n) {
                order.push(s.n);
            }
        }
        let mut map = std::collections::HashMap::new();
        let mut items = Vec::with_capacity(self.items.len());
        for (i, old) in order.into_iter().enumerate() {
            let mut s = self.items[old - 1].clone();
            map.insert(old, i + 1);
            s.n = i + 1;
            items.push(s);
        }
        self.items = items;
        map
    }

    pub fn pages_read(&self) -> usize {
        self.items.iter().filter(|s| s.read).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_equivalent_urls() {
        let mut r = SourceRegistry::new();
        let a = r.add("A", "https://www.example.com/x/?utm_source=foo#top", "", SourceKind::Search);
        let b = r.add("A page", "https://example.com/x", "", SourceKind::Page);
        assert_eq!(a, b);
        assert_eq!(r.len(), 1);
        assert!(r.get(1).unwrap().read);
        assert_eq!(r.get(1).unwrap().title, "A page");
    }

    #[test]
    fn strips_tracking_but_keeps_real_params() {
        assert_eq!(strip_tracking("https://www.canva.com/?msockid=3d17"), "https://www.canva.com/");
        assert_eq!(strip_tracking("https://x.com/p?id=3&utm_source=a"), "https://x.com/p?id=3");
        let mut r = SourceRegistry::new();
        r.add("C", "https://www.canva.com/?msockid=abc", "", SourceKind::Search);
        assert_eq!(r.get(1).unwrap().url, "https://www.canva.com/");
    }

    #[test]
    fn renumbers_in_citation_order() {
        let mut r = SourceRegistry::new();
        for u in ["https://a.com", "https://b.com", "https://c.com"] {
            r.add(u, u, "", SourceKind::Search);
        }
        let map = r.renumber(&[3, 1]);
        assert_eq!(map[&3], 1);
        assert_eq!(map[&1], 2);
        assert_eq!(map[&2], 3);
        assert_eq!(r.get(1).unwrap().url, "https://c.com");
        assert_eq!(r.get(3).unwrap().n, 3);
    }

    #[test]
    fn keeps_meaningful_query() {
        assert_eq!(normalize_url("https://x.com/p?id=3&utm_medium=a"), "x.com/p?id=3");
    }
}
