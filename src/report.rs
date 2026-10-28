//! Markdown post-processing (clickable citations, Sources section) and report storage.

use std::{collections::BTreeSet, path::PathBuf};

use anyhow::Context;
use regex::Regex;

use crate::{engine::ResearchResult, sources::SourceRegistry};

static CITE: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"\[(\d{1,3}(?:\s*[,;]\s*\d{1,3})*)\](?:\[(\d{1,3})\])*").unwrap());
static TRAILING_SOURCES: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?ims)^\s*(?:#{1,6}\s*|\*\*)\s*(sources|references|citations|bibliography|links|further reading)\b[^\n]*\n(?:\s*(?:[-*]|\d+\.|\[\d+\])[^\n]*\n?|\s*\n)*\z")
        .unwrap()
});

/// Remove a Sources/References list the model may have appended (we generate our own).
/// Only strips it if it is the very last block and consists of list lines.
pub fn strip_model_sources(md: &str) -> String {
    // a trailing "Citations: [1][2][3]" / "Sources: …" line that only repeats citations
    static CITE_LINE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\n\s*(?:\*\*)?(?:citations|sources|references)(?:\*\*)?\s*:?(?:\*\*)?\s*(?:\[[^\n]*)?\s*\z")
            .unwrap()
    });
    let md = CITE_LINE.replace(md.trim_end(), "").into_owned();
    let md = md.trim_end().to_string() + "\n";
    // Keep quick-mode "**Links**" lists: they are content, not a bibliography.
    let out = TRAILING_SOURCES.replace(&md, |caps: &regex::Captures| {
        if caps[1].eq_ignore_ascii_case("links") && caps[0].trim_start().starts_with("**") {
            caps[0].to_string()
        } else {
            String::new()
        }
    });
    out.trim_end().to_string()
}

/// Apply `f` to every line outside fenced code blocks.
fn map_prose_lines(md: &str, mut f: impl FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(md.len() + 64);
    let mut in_code = false;
    for line in md.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            out.push_str(line);
        } else if in_code {
            out.push_str(line);
        } else {
            out.push_str(&f(line));
        }
    }
    out
}

/// Rewrite the citation styles small models drift into as plain `[n]`:
/// `[Source 3]`, `[source: 2, 4]`, `【3】`, `[3†L1-L5]`, `[^3]`, and short ranges `[2-4]`.
pub fn normalize_citations(md: &str) -> String {
    static WORDY: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\[\s*(?:sources?|refs?|references?|citations?)\s*:?\s*#?(\d{1,3}(?:\s*(?:,|;|and)\s*#?\d{1,3})*)\s*\]")
            .unwrap()
    });
    static FANCY: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"【(\d{1,3})(?:†[^】]*)?】|\[(\d{1,3})†[^\]]*\]|\[\^(\d{1,3})\]").unwrap()
    });
    static RANGE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\[(\d{1,3})\s*[-–]\s*(\d{1,3})\]").unwrap());
    map_prose_lines(md, |line| {
        let l = WORDY.replace_all(line, |c: &regex::Captures| {
            let nums: Vec<&str> = c[1].split(|ch: char| !ch.is_ascii_digit()).filter(|x| !x.is_empty()).collect();
            nums.iter().map(|n| format!("[{n}]")).collect::<String>()
        });
        let l = FANCY.replace_all(&l, |c: &regex::Captures| {
            let n = c.get(1).or(c.get(2)).or(c.get(3)).map(|m| m.as_str()).unwrap_or("");
            format!("[{n}]")
        });
        let l = RANGE.replace_all(&l, |c: &regex::Captures| {
            let (a, b): (usize, usize) = (c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0));
            // `[a-z]`-like text or a long range is not a citation group; leave it alone.
            if a >= 1 && b > a && b - a <= 5 {
                (a..=b).map(|n| format!("[{n}]")).collect::<String>()
            } else {
                c[0].to_string()
            }
        });
        l.into_owned()
    })
}

/// Every URL that appears in a piece of text (tool output, evidence).
pub fn urls_in(text: &str) -> Vec<String> {
    static URL: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r#"https?://[^\s<>"'`\)\]]+"#).unwrap());
    URL.find_iter(text).map(|m| m.as_str().trim_end_matches(['.', ',', ';', ':']).to_string()).collect()
}

/// Remove Markdown links whose URL the research never saw (the model made it up),
/// keeping the link text. Returns the cleaned text and how many links were removed.
pub fn unlink_unknown(md: &str, known: &std::collections::HashSet<String>) -> (String, usize) {
    static LINK: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(!?)\[([^\]\n]*)\]\((https?://[^)\s]+)\)").unwrap());
    let mut removed = 0;
    let out = map_prose_lines(md, |line| {
        LINK.replace_all(line, |c: &regex::Captures| {
            let url = &c[3];
            if known.contains(&crate::sources::normalize_url(url)) {
                c[0].to_string()
            } else {
                removed += 1;
                if c[1].is_empty() { c[2].to_string() } else { String::new() }
            }
        })
        .into_owned()
    });
    (out, removed)
}

/// Undo the loops small models sometimes fall into: drop paragraphs that repeat an earlier one
/// and lines that repeat the line just before them (code blocks are left alone).
pub fn collapse_repeats(md: &str) -> (String, bool) {
    let mut out: Vec<String> = vec![];
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut changed = false;
    let mut in_code = false;
    for para in md.split("\n\n") {
        let fences = para.lines().filter(|l| l.trim_start().starts_with("```")).count();
        let was_code = in_code;
        if fences % 2 == 1 {
            in_code = !in_code;
        }
        if was_code || fences > 0 {
            out.push(para.to_string());
            continue;
        }
        let key: String = para.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        if key.len() > 40 && !seen.insert(key) {
            changed = true;
            continue;
        }
        let mut lines: Vec<&str> = vec![];
        for l in para.lines() {
            if lines.last().is_some_and(|p| p.trim() == l.trim() && l.trim().len() > 3 && !l.trim().starts_with('|')) {
                changed = true;
                continue;
            }
            lines.push(l);
        }
        out.push(lines.join("\n"));
    }
    (out.join("\n\n"), changed)
}

/// Tidy an answer that hit the token limit: drop a dangling half-sentence and close an open code fence.
pub fn finish_truncated(md: &str) -> String {
    let mut s = md.trim_end().to_string();
    let open_fence = s.lines().filter(|l| l.trim_start().starts_with("```")).count() % 2 == 1;
    if open_fence {
        s.push_str("\n```");
        return s;
    }
    if let Some(i) = s.rfind('\n') {
        let last = s[i + 1..].trim();
        let complete = last.ends_with(['.', '!', '?', ':', ')', '|', '`', '*', ']']) || last.starts_with('#');
        if !complete && s[..i].trim().len() > 200 {
            s.truncate(i);
        }
    }
    s.trim_end().to_string()
}

/// Turn `[3]`, `[1, 2]` and `[1][4]` into clickable `[[3]](url)` links.
/// Numbers that don't match a source are dropped. Marks used sources as cited.
/// Existing Markdown links `[text](url)` are untouched.
///
/// With `allowed`, only those numbers are kept: a citation to a source the writer was never shown
/// is as made up as one to a source that doesn't exist.
pub fn link_citations(
    md: &str,
    reg: &mut SourceRegistry,
    allowed: Option<&std::collections::HashSet<usize>>,
) -> String {
    static DIGITS: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"\d+").unwrap());
    let mut out = String::with_capacity(md.len() + 256);
    let mut in_code = false;
    // process line by line so we can skip fenced code blocks and inline code
    for line in md.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            out.push_str(line);
            continue;
        }
        if in_code {
            out.push_str(line);
            continue;
        }
        let mut last = 0;
        for m in CITE.find_iter(line) {
            let after = &line[m.end()..];
            // `[text](url)` link or `[x]:` reference definition — leave alone
            if after.starts_with('(') || after.starts_with(':') {
                continue;
            }
            // inside inline `code`? (odd number of backticks before the match)
            if line[..m.start()].matches('`').count() % 2 == 1 {
                continue;
            }
            let nums: Vec<usize> = DIGITS.find_iter(m.as_str()).filter_map(|d| d.as_str().parse().ok()).collect();
            let mut links = vec![];
            let mut seen = BTreeSet::new();
            for n in nums {
                if !seen.insert(n) {
                    continue;
                }
                if allowed.is_some_and(|a| !a.contains(&n)) {
                    continue;
                }
                if let Some(s) = reg.get(n) {
                    links.push(format!("[[{n}]]({})", s.url));
                    reg.mark_cited(n);
                }
            }
            out.push_str(&line[last..m.start()]);
            out.push_str(&links.join(""));
            last = m.end();
        }
        out.push_str(&line[last..]);
    }
    out
}

static LINKED: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"\[\[(\d+)\]\]\(").unwrap());

/// Citation numbers in the order they first appear in linked Markdown (`[[n]](url)`).
pub fn citation_order(md: &str) -> Vec<usize> {
    let mut out = vec![];
    for c in LINKED.captures_iter(md) {
        if let Ok(n) = c[1].parse::<usize>()
            && !out.contains(&n)
        {
            out.push(n);
        }
    }
    out
}

/// Rewrite linked citation numbers with an old → new mapping.
pub fn remap_citations(md: &str, map: &std::collections::HashMap<usize, usize>) -> String {
    LINKED
        .replace_all(md, |c: &regex::Captures| {
            let n: usize = c[1].parse().unwrap_or(0);
            format!("[[{}]](", map.get(&n).copied().unwrap_or(n))
        })
        .into_owned()
}

/// The uncited sources worth listing under *Further reading*: pages read and Wikipedia first,
/// then other on-topic results.
pub fn further_reading(
    reg: &SourceRegistry,
    extra: usize,
    on_topic: &dyn Fn(&crate::sources::Source) -> bool,
) -> Vec<usize> {
    let all = reg.all();
    let mut out: Vec<usize> = vec![];
    let first =
        all.iter().filter(|s| !s.cited && on_topic(s) && (s.read || s.kind == crate::sources::SourceKind::Wiki));
    for s in first.chain(all.iter().filter(|s| !s.cited && !s.read && on_topic(s))) {
        if out.len() >= extra {
            break;
        }
        if !out.contains(&s.n) {
            out.push(s.n);
        }
    }
    out
}

/// Plain text from Markdown: citation links become `[n]`, links `text (url)`, images `[image: alt]`;
/// heading marks, emphasis, quote marks and table rules are dropped. Code keeps its text.
pub fn md_to_text(md: &str) -> String {
    static IMG: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r#"!\[([^\]\n]*)\]\([^)\s]+(?:\s+"[^"]*")?\)"#).unwrap());
    static CITE_LINK: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\[\[(\d+)\]\]\([^)\s]+\)").unwrap());
    static LINK: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r#"\[([^\]\n]+)\]\(([^)\s]+)(?:\s+"[^"]*")?\)"#).unwrap());
    static EMPH: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(\*\*|__)([^*_\n]+)(\*\*|__)").unwrap());
    static RULE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"^\s*\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)*\|?\s*$").unwrap());
    let mut out = String::with_capacity(md.len());
    let mut in_code = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if RULE.is_match(line) || line.trim() == "---" || line.trim() == "***" {
            continue;
        }
        let l = line.trim_start_matches(['#', '>']).trim_start();
        let l = if line.trim_start().starts_with('#') || line.trim_start().starts_with('>') { l } else { line };
        let l = IMG.replace_all(l, |c: &regex::Captures| {
            if c[1].trim().is_empty() { String::new() } else { format!("[image: {}]", c[1].trim()) }
        });
        let l = CITE_LINK.replace_all(&l, "[$1]");
        let l = LINK.replace_all(
            &l,
            |c: &regex::Captures| {
                if c[1] == c[2] { c[1].to_string() } else { format!("{} ({})", &c[1], &c[2]) }
            },
        );
        let l = EMPH.replace_all(&l, "$2");
        let l = l.replace('`', "");
        let l = if l.trim_start().starts_with('|') { l.trim().trim_matches('|').replace(" | ", "  ·  ") } else { l };
        out.push_str(l.trim_end());
        out.push('\n');
    }
    tidy_blank_lines(&out)
}

fn tidy_blank_lines(s: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for l in s.lines() {
        if l.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l);
        out.push('\n');
    }
    out.trim().to_string()
}

/// An answer without its generated Sources section, citation links reduced to `[n]`
/// (compact context for a follow-up).
pub fn answer_body(md: &str) -> String {
    static CITE_LINK: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\[\[(\d+)\]\]\([^)\s]+\)").unwrap());
    let body = md.split("\n---\n\n## Sources").next().unwrap_or(md);
    CITE_LINK.replace_all(body, "[$1]").trim().to_string()
}

/// Build the "Sources" section: cited sources first, then a few useful uncited ones
/// (only those `on_topic` accepts, so off-topic search results don't pad the list).
pub fn sources_section(
    reg: &SourceRegistry,
    extra: usize,
    on_topic: &dyn Fn(&crate::sources::Source) -> bool,
) -> String {
    let all = reg.all();
    if all.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n---\n\n## Sources\n\n");
    let cited: Vec<_> = all.iter().filter(|s| s.cited).collect();
    for s in &cited {
        out.push_str(&line(s));
    }
    let picks = further_reading(reg, extra, on_topic);
    let further: Vec<_> = picks.iter().filter_map(|n| reg.get(*n)).collect();
    if !further.is_empty() {
        if !cited.is_empty() {
            out.push_str("\n**Further reading**\n\n");
        }
        let mut seen = BTreeSet::new();
        for s in further {
            if seen.insert(s.n) {
                out.push_str(&line(s));
            }
        }
    }
    out
}

fn line(s: &crate::sources::Source) -> String {
    let title = s.title.replace('[', "(").replace(']', ")");
    let date = s
        .date
        .as_deref()
        .map(|d| {
            // ISO timestamps → the date part; human dates ("Sep 30, 2025", "4 days ago") as they are
            let iso = d.len() >= 10 && d.as_bytes()[..4].iter().all(u8::is_ascii_digit) && d.as_bytes()[4] == b'-';
            format!(" · {}", if iso { d.chars().take(10).collect::<String>() } else { d.chars().take(24).collect() })
        })
        .unwrap_or_default();
    format!("{}. [{title}]({}) — `{}`{date}\n", s.n, s.url, s.domain)
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() { "report".into() } else { s }
}

fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Markdown file content with YAML front-matter.
pub fn to_file(r: &ResearchResult) -> String {
    format!(
        "---\nid: {}\nquery: {}\ndepth: {}\nintent: {}\nmodel: {}\ncreated_at: {}\nduration_ms: {}\nsources: {}\n---\n\n{}\n",
        r.id,
        yaml_str(&r.query),
        r.depth.as_str(),
        serde_json::to_value(r.intent).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
        yaml_str(&r.model),
        r.created_at.to_rfc3339(),
        r.stats.duration_ms,
        r.sources.len(),
        r.markdown.trim_end()
    )
}

/// Saves reports as `<reports_dir>/<id>.md` plus a `<id>.json` sidecar with full metadata.
#[derive(Clone)]
pub struct ReportStore {
    pub dir: PathBuf,
    /// Serialises read-modify-write of `<id>.pages.json`.
    pages_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl ReportStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, pages_lock: Default::default() }
    }

    fn valid_id(id: &str) -> bool {
        !id.is_empty() && id.len() < 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    pub async fn save(&self, r: &ResearchResult) -> anyhow::Result<PathBuf> {
        tokio::fs::create_dir_all(&self.dir).await.with_context(|| format!("create {}", self.dir.display()))?;
        let md_path = self.dir.join(format!("{}.md", r.id));
        tokio::fs::write(&md_path, to_file(r)).await?;
        tokio::fs::write(self.dir.join(format!("{}.json", r.id)), serde_json::to_vec_pretty(r)?).await?;
        Ok(md_path)
    }

    pub async fn get(&self, id: &str) -> Option<ResearchResult> {
        if !Self::valid_id(id) {
            return None;
        }
        let bytes = tokio::fs::read(self.dir.join(format!("{id}.json"))).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub async fn markdown(&self, id: &str) -> Option<String> {
        if !Self::valid_id(id) {
            return None;
        }
        tokio::fs::read_to_string(self.dir.join(format!("{id}.md"))).await.ok()
    }

    pub async fn delete(&self, id: &str) -> bool {
        if !Self::valid_id(id) {
            return false;
        }
        let a = tokio::fs::remove_file(self.dir.join(format!("{id}.md"))).await.is_ok();
        let b = tokio::fs::remove_file(self.dir.join(format!("{id}.json"))).await.is_ok();
        let _ = tokio::fs::remove_file(self.dir.join(format!("{id}.pages.json"))).await;
        a || b
    }

    /// Full text (and images) of a report's sources, by citation number: `<id>.pages.json`.
    pub async fn pages(&self, id: &str) -> std::collections::HashMap<usize, crate::tools::extract::Page> {
        if !Self::valid_id(id) {
            return Default::default();
        }
        match tokio::fs::read(self.dir.join(format!("{id}.pages.json"))).await {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
            Err(_) => Default::default(),
        }
    }

    pub async fn save_pages(
        &self,
        id: &str,
        pages: &std::collections::HashMap<usize, crate::tools::extract::Page>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(Self::valid_id(id), "invalid report id");
        let tmp = self.dir.join(format!("{id}.pages.json.tmp"));
        tokio::fs::write(&tmp, serde_json::to_vec(pages)?).await?;
        // write-then-rename: a crash never leaves a half-written file behind
        tokio::fs::rename(&tmp, self.dir.join(format!("{id}.pages.json"))).await?;
        Ok(())
    }

    /// Add one page (read later, on request) to a report's stored pages.
    pub async fn add_page(&self, id: &str, n: usize, page: &crate::tools::extract::Page) -> anyhow::Result<()> {
        let _guard = self.pages_lock.lock().await;
        let mut all = self.pages(id).await;
        all.insert(n, page.clone());
        self.save_pages(id, &all).await
    }

    /// Newest first.
    pub async fn list(&self, limit: usize) -> Vec<serde_json::Value> {
        let mut out = vec![];
        let Ok(mut rd) = tokio::fs::read_dir(&self.dir).await else { return out };
        while let Ok(Some(e)) = rd.next_entry().await {
            let p = e.path();
            let name = p.file_name().and_then(|x| x.to_str()).unwrap_or("");
            if !name.ends_with(".json") || name.ends_with(".pages.json") {
                continue;
            }
            if let Ok(bytes) = tokio::fs::read(&p).await
                && let Ok(r) = serde_json::from_slice::<ResearchResult>(&bytes)
            {
                out.push(serde_json::json!({
                    "id": r.id,
                    "query": r.query,
                    "depth": r.depth,
                    "intent": r.intent,
                    "created_at": r.created_at,
                    "sources": r.sources.len(),
                    "words": r.markdown.split_whitespace().count(),
                }));
            }
        }
        out.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
        out.truncate(limit);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::SourceKind;

    fn reg() -> SourceRegistry {
        let mut r = SourceRegistry::new();
        r.add("One", "https://one.com", "", SourceKind::Search);
        r.add("Two", "https://two.com", "", SourceKind::Page);
        r.add("Three", "https://three.com", "", SourceKind::Search);
        r
    }

    #[test]
    fn links_citations() {
        let mut r = reg();
        let out =
            link_citations("Fact A [1]. Fact B [2, 3]. Bad [9]. Link [x](https://y.z). Pair [1][3].", &mut r, None);
        assert!(out.contains("Fact A [[1]](https://one.com)."));
        assert!(out.contains("[[2]](https://two.com)[[3]](https://three.com)"));
        assert!(out.contains("Bad ."));
        assert!(out.contains("[x](https://y.z)"));
        assert!(out.contains("Pair [[1]](https://one.com)[[3]](https://three.com)"));
        assert!(r.get(1).unwrap().cited && r.get(2).unwrap().cited);
    }

    #[test]
    fn skips_code_blocks() {
        let mut r = reg();
        let out = link_citations("```\nlet x = a[1];\n```\nok [1]", &mut r, None);
        assert!(out.contains("let x = a[1];"));
        let allowed: std::collections::HashSet<usize> = [2].into();
        let only = link_citations("a [1] b [2]", &mut r, Some(&allowed));
        assert_eq!(only, "a  b [[2]](https://two.com)");
        assert!(out.contains("let x = a[1];"));
        assert!(out.contains("ok [[1]](https://one.com)"));
    }

    #[test]
    fn strips_trailing_sources_but_keeps_links_list() {
        let md = "Body text.\n\n## Sources\n- [1] One\n- [2] Two\n";
        assert_eq!(strip_model_sources(md), "Body text.");
        assert_eq!(strip_model_sources("Body [1].\n\nCitations: [1][2](https://x.y)[3]\n"), "Body [1].");
        let quick = "Answer.\n\n**Links**\n- [A](https://a.com) — x\n";
        assert!(strip_model_sources(quick).contains("**Links**"));
    }

    #[test]
    fn sources_section_lists_cited_first() {
        let mut r = reg();
        r.mark_cited(3);
        let s = sources_section(&r, 2, &|_| true);
        let i3 = s.find("three.com").unwrap();
        let i2 = s.find("two.com").unwrap();
        assert!(i3 < i2);
        assert!(s.contains("Further reading"));
    }

    #[test]
    fn normalizes_citation_styles() {
        let out = normalize_citations(
            "A [Source 1]. B [sources: 2, 3]. C 【4†L1】. D [^5]. E [1-3]. F [a-z].\n```\nx[Source 1]\n```\n",
        );
        assert!(out.contains("A [1]."));
        assert!(out.contains("B [2][3]."));
        assert!(out.contains("C [4]."));
        assert!(out.contains("D [5]."));
        assert!(out.contains("E [1][2][3]."));
        assert!(out.contains("F [a-z]."));
        assert!(out.contains("x[Source 1]"), "code untouched");
    }

    #[test]
    fn unlinks_made_up_urls() {
        let known: std::collections::HashSet<String> =
            ["https://real.com/a"].iter().map(|u| crate::sources::normalize_url(u)).collect();
        let (out, n) =
            unlink_unknown("- [Real](https://www.real.com/a/) ok\n- [Fake](https://fake.example/x) — nope\n", &known);
        assert_eq!(n, 1);
        assert!(out.contains("[Real](https://www.real.com/a/)"));
        assert!(out.contains("- Fake — nope"));
    }

    #[test]
    fn collapses_loops() {
        let p = "This paragraph is long enough to count as a real repeated paragraph.";
        let md = format!("{p}\n\nOther.\n\n{p}\n\n- same line\n- same line\n- other");
        let (out, changed) = collapse_repeats(&md);
        assert!(changed);
        assert_eq!(out.matches(p).count(), 1);
        assert_eq!(out.matches("- same line").count(), 1);
    }

    #[test]
    fn trims_cut_off_answers() {
        let body = "A complete sentence that is long enough. ".repeat(8);
        let out = finish_truncated(&format!("{body}\nAnd then the model stopped in the mid"));
        assert!(!out.contains("mid"));
        assert!(finish_truncated("```bash\napt install x").ends_with("```"));
    }

    #[test]
    fn markdown_to_plain_text() {
        let md = "# Title\n\nSome **bold** fact [[2]](https://x.com/a) and [a link](https://y.com).\n\n\
                  ![A cat](https://i/cat.png)\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quoted\n\n```\ncode [1]\n```\n";
        let t = md_to_text(md);
        assert!(t.starts_with("Title\n\nSome bold fact [2] and a link (https://y.com)."), "{t}");
        assert!(t.contains("[image: A cat]"));
        assert!(t.contains("a  ·  b") && !t.contains("---"));
        assert!(t.contains("\nquoted\n"));
        assert!(t.contains("code [1]"));
    }

    #[test]
    fn answer_body_drops_sources() {
        let md = "Fact [[1]](https://a.com).\n\n---\n\n## Sources\n\n1. [A](https://a.com)";
        assert_eq!(answer_body(md), "Fact [1].");
    }

    #[test]
    fn slug() {
        assert_eq!(slugify("What is VisionQuest?!"), "what-is-visionquest");
        assert_eq!(slugify("???"), "report");
    }
}
