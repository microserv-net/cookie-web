//! Content extraction: HTML → clean Markdown (readability), links, PDF text,
//! and query-focused excerpting so small models only see what matters.

use std::collections::HashSet;

use dom_smoothie::{Config as RConfig, Readability, TextMode};
use scraper::{Html, Selector};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Link {
    pub url: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub markdown: String,
    pub published: Option<String>,
    pub byline: Option<String>,
    pub site_name: Option<String>,
    pub content_type: String,
    #[serde(skip)]
    pub links: Vec<Link>,
}

const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "iframe", "svg", "canvas", "form", "nav", "footer", "header", "aside", "button",
    "template", "head",
];

/// Convert an HTML document into a [`Page`]. CPU-bound; call from `spawn_blocking`.
pub fn html_to_page(html: &str, url: &str, content_type: &str) -> Page {
    let links = extract_links(html, url);
    let cfg = RConfig { text_mode: TextMode::Markdown, ..Default::default() };
    let article = Readability::new(html, Some(url), Some(cfg)).ok().and_then(|mut r| r.parse().ok());

    let fallback_title = || {
        let doc = Html::parse_document(html);
        let sel = Selector::parse("title").unwrap();
        doc.select(&sel).next().map(|t| t.text().collect::<String>().trim().to_string()).unwrap_or_default()
    };

    match article {
        Some(a) if a.text_content.trim().len() >= 200 => Page {
            url: url.to_string(),
            title: if a.title.trim().is_empty() { fallback_title() } else { a.title.trim().to_string() },
            markdown: tidy_markdown(&a.text_content),
            published: a.published_time.clone(),
            byline: a.byline.clone(),
            site_name: a.site_name.clone(),
            content_type: content_type.to_string(),
            links,
        },
        _ => {
            let conv = htmd::HtmlToMarkdown::builder().skip_tags(SKIP_TAGS.to_vec()).build();
            let md = conv.convert(html).unwrap_or_default();
            Page {
                url: url.to_string(),
                title: fallback_title(),
                markdown: tidy_markdown(&md),
                published: None,
                byline: None,
                site_name: None,
                content_type: content_type.to_string(),
                links,
            }
        }
    }
}

/// PDF bytes → plain text page. CPU-bound; call from `spawn_blocking`.
pub fn pdf_to_page(bytes: &[u8], url: &str) -> Option<Page> {
    let text = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes)).ok()?.ok()?;
    let text = tidy_markdown(&text);
    if text.trim().is_empty() {
        return None;
    }
    let title = pdf_title(&text, url);
    Some(Page {
        url: url.to_string(),
        title,
        markdown: text,
        published: None,
        byline: None,
        site_name: None,
        content_type: "application/pdf".into(),
        links: vec![],
    })
}

/// Guess a PDF's title: the first short, title-like line near the top that isn't boilerplate.
fn pdf_title(text: &str, url: &str) -> String {
    const BOILER: &[&str] = &[
        "permission",
        "copyright",
        "licen",
        "arxiv",
        "preprint",
        "http",
        "doi",
        "©",
        "journal",
        "proceedings",
        "page ",
    ];
    text.lines()
        .take(40)
        .map(str::trim)
        .find(|l| {
            let words = l.split_whitespace().count();
            let low = l.to_lowercase();
            (2..=16).contains(&words)
                && l.len() <= 140
                && l.chars().next().is_some_and(|c| c.is_uppercase() || c.is_numeric())
                && !l.ends_with('.')
                && !BOILER.iter().any(|b| low.contains(b))
        })
        .map(String::from)
        .unwrap_or_else(|| {
            url.rsplit('/')
                .find(|s| !s.is_empty())
                .map(|s| format!("PDF: {s}"))
                .unwrap_or_else(|| "PDF document".into())
        })
}

/// Collapse whitespace noise and runs of blank lines.
pub fn tidy_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let l = line.trim_end();
        if l.trim().is_empty() {
            blank += 1;
            if blank <= 1 {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(l);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// All `<a href>` links in a document, absolutised and de-duplicated.
pub fn extract_links(html: &str, base: &str) -> Vec<Link> {
    let Ok(base) = url::Url::parse(base) else { return vec![] };
    let doc = Html::parse_document(html);
    let sel = Selector::parse("a[href]").unwrap();
    let mut seen = HashSet::new();
    let mut out = vec![];
    for a in doc.select(&sel) {
        let href = a.value().attr("href").unwrap_or("").trim();
        if href.is_empty()
            || href.starts_with('#')
            || href.starts_with("javascript:")
            || href.starts_with("mailto:")
            || href.starts_with("tel:")
        {
            continue;
        }
        let Ok(mut abs) = base.join(href) else { continue };
        if !matches!(abs.scheme(), "http" | "https") {
            continue;
        }
        abs.set_fragment(None);
        let u = abs.to_string();
        if seen.insert(u.clone()) {
            let text: String = a.text().collect::<Vec<_>>().join(" ");
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            out.push(Link { url: u, text: text.chars().take(120).collect() });
        }
    }
    out
}

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "is", "are", "was", "were", "be", "it", "this",
    "that", "with", "as", "by", "at", "from", "what", "how", "why", "who", "when", "where", "which", "do", "does", "i",
    "me", "my", "you", "your", "about", "can", "should", "vs", "versus", "best",
];

pub fn terms(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1 && !STOPWORDS.contains(w))
        .map(String::from)
        .collect()
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    if let Some(p) = t.rfind(['\n', '.', ' '])
        && p > max / 2
    {
        t.truncate(p + 1);
    }
    t.push_str("\n…[truncated]");
    t
}

/// Pick the blocks of `md` most relevant to `focus`, preserving document order,
/// within `max_chars`. Without a focus (or if the page is short) returns the head.
pub fn focus_excerpt(md: &str, focus: Option<&str>, max_chars: usize) -> String {
    if md.chars().count() <= max_chars {
        return md.to_string();
    }
    let q: Vec<String> = focus.map(terms).unwrap_or_default();
    if q.is_empty() {
        return truncate_chars(md, max_chars);
    }
    // Split into blocks (paragraphs / list groups), merging tiny ones with their neighbour.
    let mut blocks: Vec<String> = vec![];
    for b in md.split("\n\n") {
        let b = b.trim();
        if b.is_empty() {
            continue;
        }
        match blocks.last_mut() {
            Some(last) if last.len() < 160 => {
                last.push_str("\n\n");
                last.push_str(b);
            }
            _ => blocks.push(b.to_string()),
        }
    }
    let n_blocks = blocks.len().max(1) as f64;
    // document frequency per term, for idf weighting
    let df: Vec<f64> =
        q.iter().map(|t| blocks.iter().filter(|b| b.to_lowercase().contains(t.as_str())).count() as f64).collect();
    let mut scored: Vec<(usize, f64)> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let low = b.to_lowercase();
            let len_norm = 1.0 + (low.len() as f64 / 600.0);
            let mut s = 0.0;
            for (t, d) in q.iter().zip(&df) {
                let tf = low.matches(t.as_str()).count() as f64;
                if tf > 0.0 {
                    let idf = ((n_blocks + 1.0) / (d + 0.5)).ln().max(0.1);
                    s += idf * (tf * 2.2) / (tf + 1.2 * len_norm);
                }
            }
            // the lead of a document is usually a good summary
            if i < 2 {
                s += 0.5;
            }
            (i, s)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut chosen: Vec<usize> = vec![];
    let mut used = 0;
    for (i, s) in scored {
        if s <= 0.0 && !chosen.is_empty() {
            break;
        }
        let len = blocks[i].chars().count() + 6;
        if used + len > max_chars {
            continue;
        }
        used += len;
        chosen.push(i);
    }
    chosen.sort_unstable();
    let mut out = String::new();
    let mut prev: Option<usize> = None;
    for i in chosen {
        if let Some(p) = prev {
            out.push_str(if i == p + 1 { "\n\n" } else { "\n\n[…]\n\n" });
        }
        out.push_str(&blocks[i]);
        prev = Some(i);
    }
    if out.is_empty() { truncate_chars(md, max_chars) } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = r#"<html><head><title>Cookie Facts</title></head><body>
      <nav><a href="/home">Home</a></nav>
      <article><h1>Cookie Facts</h1>
      <p>Chocolate chip cookies were invented by Ruth Wakefield around 1938 at the Toll House Inn in Whitman, Massachusetts. They quickly became one of the most popular cookies in the United States.</p>
      <p>The original recipe used chopped semi-sweet chocolate from a Nestlé bar. Nestlé later printed the recipe on its packaging, which spread it widely across American kitchens.</p>
      <p>See <a href="https://example.org/recipe#x">the recipe</a> and <a href="mailto:a@b.c">mail</a>.</p>
      </article></body></html>"#;

    #[test]
    fn extracts_article_markdown_and_links() {
        let p = html_to_page(HTML, "https://example.com/cookies", "text/html");
        assert_eq!(p.title, "Cookie Facts");
        assert!(p.markdown.contains("Ruth Wakefield"));
        assert!(!p.markdown.contains("<p>"));
        let urls: Vec<_> = p.links.iter().map(|l| l.url.as_str()).collect();
        assert!(urls.contains(&"https://example.org/recipe"));
        assert!(urls.contains(&"https://example.com/home"));
        assert!(!urls.iter().any(|u| u.starts_with("mailto")));
    }

    #[test]
    fn focus_picks_relevant_blocks() {
        let mut md = String::new();
        for i in 0..40 {
            md.push_str(&format!(
                "Filler paragraph number {i} talking about weather and gardening in general terms.\n\n"
            ));
        }
        md.push_str("The WireGuard handshake uses Noise_IK with Curve25519 keys.\n\n");
        for i in 40..80 {
            md.push_str(&format!("More filler {i} about unrelated cooking topics and other things entirely.\n\n"));
        }
        let ex = focus_excerpt(&md, Some("wireguard handshake"), 600);
        assert!(ex.contains("Noise_IK"));
        assert!(ex.chars().count() <= 700);
    }

    #[test]
    fn truncates_on_boundary() {
        let t = truncate_chars("hello world this is long", 12);
        assert!(t.starts_with("hello world"));
        assert!(t.ends_with("[truncated]"));
    }
}
