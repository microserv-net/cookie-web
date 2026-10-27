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
pub fn link_citations(md: &str, reg: &mut SourceRegistry) -> String {
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

/// Build the "Sources" section: cited sources first, then a few useful uncited ones.
pub fn sources_section(reg: &SourceRegistry, extra: usize) -> String {
    let all = reg.all();
    if all.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n---\n\n## Sources\n\n");
    let cited: Vec<_> = all.iter().filter(|s| s.cited).collect();
    for s in &cited {
        out.push_str(&line(s));
    }
    let further: Vec<_> = all
        .iter()
        .filter(|s| !s.cited)
        .filter(|s| s.read || matches!(s.kind, crate::sources::SourceKind::Wiki))
        .chain(all.iter().filter(|s| !s.cited && !s.read))
        .take(extra)
        .collect();
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
    let date = s.date.as_deref().map(|d| format!(" · {}", d.chars().take(10).collect::<String>())).unwrap_or_default();
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
}

impl ReportStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
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
        a || b
    }

    /// Newest first.
    pub async fn list(&self, limit: usize) -> Vec<serde_json::Value> {
        let mut out = vec![];
        let Ok(mut rd) = tokio::fs::read_dir(&self.dir).await else { return out };
        while let Ok(Some(e)) = rd.next_entry().await {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
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
        let out = link_citations("Fact A [1]. Fact B [2, 3]. Bad [9]. Link [x](https://y.z). Pair [1][3].", &mut r);
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
        let out = link_citations("```\nlet x = a[1];\n```\nok [1]", &mut r);
        assert!(out.contains("let x = a[1];"));
        assert!(out.contains("ok [[1]](https://one.com)"));
    }

    #[test]
    fn strips_trailing_sources_but_keeps_links_list() {
        let md = "Body text.\n\n## Sources\n- [1] One\n- [2] Two\n";
        assert_eq!(strip_model_sources(md), "Body text.");
        let quick = "Answer.\n\n**Links**\n- [A](https://a.com) — x\n";
        assert!(strip_model_sources(quick).contains("**Links**"));
    }

    #[test]
    fn sources_section_lists_cited_first() {
        let mut r = reg();
        r.mark_cited(3);
        let s = sources_section(&r, 2);
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
    fn slug() {
        assert_eq!(slugify("What is VisionQuest?!"), "what-is-visionquest");
        assert_eq!(slugify("???"), "report");
    }
}
