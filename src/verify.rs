//! Fact check: catch statements that connect the wrong names.
//!
//! Small models read correct sources and still blend similar subjects ("F.R.I.D.A.Y. is Allison Hogan,
//! E.D.I.T.H. is Edith Marks" comes out as "Friday is based on Edith Marks"). After writing, every
//! statement that links two or more names is checked against the source sentences that mention those
//! names, one short question each ("do these passages support this statement?"). Statements the
//! sources contradict or don't contain are removed, and the removals are reported in `warnings`.

use serde_json::json;

use crate::{
    engine::{Evidence, Run},
    llm::{ChatRequest, Message, Tier, parse_json_loose},
};

/// Characters of source text shown for one check.
const PASSAGE_CHARS: usize = 1_800;

/// A sentence or list item of the answer, as a byte range of the Markdown.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub start: usize,
    pub end: usize,
}

/// Words that start sentences or are too generic to count as names.
const NOT_NAMES: &[&str] = &[
    "the",
    "a",
    "an",
    "in",
    "on",
    "at",
    "this",
    "that",
    "these",
    "those",
    "it",
    "its",
    "he",
    "she",
    "they",
    "his",
    "her",
    "their",
    "there",
    "here",
    "while",
    "however",
    "although",
    "when",
    "where",
    "as",
    "but",
    "and",
    "or",
    "if",
    "so",
    "also",
    "both",
    "each",
    "all",
    "some",
    "many",
    "who",
    "what",
    "why",
    "how",
    "yes",
    "no",
    "not",
    "after",
    "before",
    "during",
    "since",
    "because",
    "with",
    "without",
    "for",
    "from",
    "to",
    "by",
    "of",
    "unlike",
    "like",
    "one",
    "two",
    "first",
    "second",
    "today",
    "note",
    "overall",
    "instead",
    "meanwhile",
    "according",
    "ai",
    "ais",
    "tv",
    "us",
    "uk",
    "eu",
    "faq",
    "tl",
    "dr",
    "tl;dr",
    "i",
    "you",
    "we",
    "my",
    "our",
    "your",
];

/// Lower-case, dots and possessives removed: "F.R.I.D.A.Y.'s" → "friday".
pub fn norm(s: &str) -> String {
    let s = s.to_lowercase().replace(['.', '*', '_'], "").replace('’', "'");
    let s = s.trim_end_matches("'s").trim_matches(|c: char| !c.is_alphanumeric());
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Proper names in a statement: runs of capitalised words or dotted acronyms
/// ("Orla Brady", "F.R.I.D.A.Y.", "Samuel L. Jackson"), minus sentence-start and generic words.
pub fn names(text: &str) -> Vec<String> {
    let clean = strip_markup(text);
    let mut out: Vec<String> = vec![];
    let mut run: Vec<String> = vec![];
    let flush = |run: &mut Vec<String>, out: &mut Vec<String>| {
        while run.first().is_some_and(|w| NOT_NAMES.contains(&norm(w).as_str())) {
            run.remove(0);
        }
        let name = norm(&run.join(" "));
        if name.chars().filter(|c| c.is_alphanumeric()).count() >= 3
            && !NOT_NAMES.contains(&name.as_str())
            && !out.contains(&name)
        {
            out.push(name);
        }
        run.clear();
    };
    for raw in clean.split_whitespace() {
        // an opening bracket or quote starts something new: "Happy Hogan (Tony's bodyguard)"
        if raw.starts_with(['(', '"', '“', '‘', '[']) {
            flush(&mut run, &mut out);
        }
        let w =
            raw.trim_matches(|c: char| matches!(c, '(' | ')' | '"' | '“' | '”' | '‘' | ',' | ';' | ':' | '!' | '?'));
        // a possessive ends a name: "Marvel's VisionQuest" → "Marvel", "VisionQuest"
        let possessive = w.ends_with("'s") || w.ends_with("’s");
        let starts_upper = w.chars().next().is_some_and(|c| c.is_uppercase());
        let initial = w.len() == 2 && w.ends_with('.') && starts_upper; // "L."
        if starts_upper || initial {
            run.push(w.trim_end_matches(|c: char| c == ',' || (c == '.' && !looks_dotted(w))).to_string());
            // a sentence end inside the run ("…Brady. The …") closes it
            if w.ends_with('.') && !initial && !looks_dotted(w) {
                flush(&mut run, &mut out);
            }
        } else {
            flush(&mut run, &mut out);
        }
        if raw.ends_with([',', ';', ':', '!', '?', ')', '"', '”']) || possessive {
            flush(&mut run, &mut out);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// "F.R.I.D.A.Y." / "U.S." — dotted acronyms (the dots are not sentence ends).
fn looks_dotted(w: &str) -> bool {
    let core = w.trim_end_matches('.');
    core.contains('.') && core.split('.').all(|p| p.chars().count() <= 2)
}

/// Remove citations, links and emphasis so only the statement's words remain.
pub fn strip_markup(s: &str) -> String {
    static CITE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\[\[?\d+\]\]?(?:\([^)\s]*\))?").unwrap());
    static LINK: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\[([^\]\n]+)\]\([^)\s]+\)").unwrap());
    let s = CITE.replace_all(s, "");
    let s = LINK.replace_all(&s, "$1");
    s.replace(['*', '_', '`'], "").replace(['—', '–'], " — ").split_whitespace().collect::<Vec<_>>().join(" ")
}

const ABBREV: &[&str] =
    &["mr", "mrs", "ms", "dr", "st", "vs", "etc", "eg", "ie", "jr", "sr", "no", "vol", "fig", "approx"];

/// Byte offsets where sentences end in `s` (after the punctuation and any citations that follow it).
fn sentence_ends(s: &str) -> Vec<usize> {
    let b = s.as_bytes();
    let mut ends = vec![];
    let mut i = 0;
    while i < b.len() {
        if matches!(b[i], b'.' | b'!' | b'?') {
            // the word this punctuation ends
            let word_start = s[..i].rfind(|c: char| c.is_whitespace()).map_or(0, |p| p + 1);
            let word = &s[word_start..=i];
            let bare = norm(word);
            let is_initial = word.trim_end_matches('.').chars().count() == 1;
            if b[i] == b'.' && (looks_dotted(word) || is_initial || ABBREV.contains(&bare.as_str())) {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            // citations right after the period belong to this sentence: "… fact. [3]"
            while j < b.len() && (b[j] == b' ' || b[j] == b'[' || b[j] == b']' || b[j].is_ascii_digit() || b[j] == b')')
            {
                j += 1;
            }
            let next = s[j..].chars().next();
            if next.is_none_or(|c| c.is_uppercase() || c == '*' || c == '"' || c == '“' || c == '(' || c.is_numeric())
            {
                // keep trailing spaces with the next sentence
                let mut e = i + 1;
                while e < j && b[e] != b' ' {
                    e += 1;
                }
                let e = s[i + 1..j].rfind(']').map_or(e.min(j), |p| i + 1 + p + 1).max(i + 1);
                ends.push(e);
                i = j;
                continue;
            }
        }
        i += 1;
    }
    ends
}

/// The statements of an answer: sentences of prose paragraphs and whole list items.
/// Headings, code, tables, quotes and the link lists of quick answers are left alone.
pub fn units(md: &str) -> Vec<Unit> {
    let mut out = vec![];
    let mut in_code = false;
    let mut offset = 0;
    for line in md.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let t = line.trim_start();
        if t.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code || t.trim().is_empty() || t.starts_with('#') || t.starts_with('|') || t.starts_with('>') {
            continue;
        }
        let lead = line.len() - t.len();
        let content_end = start + line.trim_end().len();
        let is_item = t.starts_with("- ")
            || t.starts_with("* ")
            || t.split_once(". ").is_some_and(|(n, _)| n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty());
        if is_item {
            out.push(Unit { start: start + lead, end: content_end });
            continue;
        }
        let text = &md[start..content_end];
        let mut s = 0;
        for e in sentence_ends(text) {
            let piece = &text[s..e];
            if !piece.trim().is_empty() {
                let lead = piece.len() - piece.trim_start().len();
                out.push(Unit { start: start + s + lead, end: start + e });
            }
            s = e;
        }
        if text[s..].trim().chars().count() > 1 {
            let lead = text[s..].len() - text[s..].trim_start().len();
            out.push(Unit { start: start + s + lead, end: content_end });
        }
    }
    out
}

/// Source sentences (normalised for matching, plus the original text).
struct SourceSentence {
    item: usize,
    norm: String,
    text: String,
}

fn source_sentences(evidence: &[Evidence]) -> Vec<SourceSentence> {
    let mut out = vec![];
    for (item, e) in evidence.iter().enumerate() {
        for line in e.text.lines() {
            let mut s = 0;
            for end in sentence_ends(line).into_iter().chain(std::iter::once(line.len())) {
                if end <= s {
                    continue;
                }
                let t = line[s..end].trim();
                s = end;
                if t.chars().count() >= 12 {
                    out.push(SourceSentence { item, norm: format!(" {} ", norm_text(t)), text: t.to_string() });
                }
            }
        }
    }
    out
}

/// Normalised text for name matching: lower-case, dots and possessives dropped, punctuation as spaces.
fn norm_text(s: &str) -> String {
    let s = s.to_lowercase().replace('.', "").replace('’', "'").replace("'s ", " ");
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Does the sentence mention the name? Multi-word names also count when all their words occur
/// ("Allison Hogan" in "Hogan, Allison"; "Samuel Jackson" in "Samuel L. Jackson").
fn mentions(sentence_norm: &str, name: &str) -> bool {
    let n = norm_text(name);
    if sentence_norm.contains(&format!(" {n} ")) {
        return true;
    }
    let words: Vec<&str> = n.split(' ').filter(|w| w.chars().count() >= 3).collect();
    words.len() >= 2 && words.iter().all(|w| sentence_norm.contains(&format!(" {w} ")))
}

/// Names that no source sentence mentions.
fn missing(sources: &[SourceSentence], names: &[String]) -> Vec<String> {
    names.iter().filter(|n| !sources.iter().any(|s| mentions(&s.norm, n))).cloned().collect()
}

/// The source sentences that best cover `names` (with their neighbours, for pronouns).
fn passage(sources: &[SourceSentence], names: &[String]) -> String {
    let mut scored: Vec<(usize, usize)> = sources
        .iter()
        .enumerate()
        .map(|(i, s)| (names.iter().filter(|n| mentions(&s.norm, n)).count(), i))
        .filter(|(c, _)| *c > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut picked: Vec<usize> = vec![];
    for (_, i) in scored.into_iter().take(6) {
        for j in [i.saturating_sub(1), i, i + 1] {
            if j < sources.len() && sources[j].item == sources[i].item && !picked.contains(&j) {
                picked.push(j);
            }
        }
    }
    picked.sort_unstable();
    let mut out = String::new();
    let mut prev: Option<usize> = None;
    for i in picked {
        let add = if prev.is_some_and(|p| p + 1 == i) {
            format!(" {}", sources[i].text)
        } else {
            format!("\n- {}", sources[i].text)
        };
        if out.len() + add.len() > PASSAGE_CHARS {
            break;
        }
        out.push_str(&add);
        prev = Some(i);
    }
    out.trim().to_string()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Verdict {
    Supported,
    Contradicted,
    NotFound,
    Unknown,
}

async fn judge(run: &Run<'_>, passage: &str, statement: &str) -> Verdict {
    let schema = json!({
        "type": "object",
        "properties": { "verdict": { "type": "string", "enum": ["supported", "contradicted", "not_in_sources"] } },
        "required": ["verdict"]
    });
    let sys = "You check one statement against source passages. Pay close attention to WHICH name each fact belongs \
               to: if the passages give a fact about a different person, character or thing than the statement does, \
               the statement is contradicted.\n\
               - supported: the passages say this (paraphrase is fine)\n\
               - contradicted: the passages say something different\n\
               - not_in_sources: the passages don't say this\n\
               Reply with JSON only.";
    let user = format!("Passages:\n{passage}\n\nStatement: {statement}");
    let req = ChatRequest::new(vec![Message::system(sys), Message::user(user)])
        .tier(Tier::Fast)
        .json(schema)
        .temperature(0.0)
        .max_tokens(30);
    match run.chat(req).await.ok().and_then(|m| parse_json_loose(&m.content)) {
        Some(v) => match v["verdict"].as_str().unwrap_or("") {
            "supported" => Verdict::Supported,
            "contradicted" => Verdict::Contradicted,
            "not_in_sources" => Verdict::NotFound,
            _ => Verdict::Unknown,
        },
        None => Verdict::Unknown,
    }
}

/// Check the statements of `body` that connect names; return the body without the unsupported ones
/// and the statements removed. `max_checks` bounds the extra model calls.
pub async fn check(run: &Run<'_>, body: &str, evidence: &[Evidence], max_checks: usize) -> (String, Vec<String>) {
    let sources = source_sentences(evidence);
    let asked: Vec<String> = names(&run.query);
    let mut candidates: Vec<(Unit, String, Vec<String>)> = units(body)
        .into_iter()
        .filter_map(|u| {
            let text = strip_markup(&body[u.start..u.end]);
            let ns = names(&text);
            (ns.len() >= 2 && text.split_whitespace().count() >= 4).then_some((u, text, ns))
        })
        .collect();
    if candidates.is_empty() {
        return (body.to_string(), vec![]);
    }
    // most names first: that's where facts get crossed
    candidates.sort_by_key(|c| std::cmp::Reverse(c.2.len()));
    candidates.truncate(max_checks);
    run.events.status(format!(
        "Checking {} statement{} against the sources",
        candidates.len(),
        if candidates.len() == 1 { "" } else { "s" }
    ));

    let mut bad: Vec<(Unit, String, Verdict)> = vec![];
    let mut checked = 0;
    for (u, text, ns) in &candidates {
        if run.out_of_time() && checked > 0 {
            break;
        }
        let gone = missing(&sources, ns);
        let present: Vec<String> = ns.iter().filter(|n| !gone.contains(n)).cloned().collect();
        let invented = gone.iter().filter(|n| !asked.contains(n)).count();
        let verdict = if present.is_empty() && invented >= 2 {
            // ties together names the research never came across
            Verdict::NotFound
        } else if present.is_empty() {
            Verdict::Unknown
        } else {
            checked += 1;
            judge(run, &passage(&sources, &present), text).await
        };
        if matches!(verdict, Verdict::Contradicted | Verdict::NotFound) {
            bad.push((u.clone(), text.clone(), verdict));
        }
    }
    // If the checker rejects most of the answer, the checker is the likelier problem:
    // only act on outright contradictions then.
    if bad.len() * 2 > candidates.len() && candidates.len() >= 3 {
        bad.retain(|b| b.2 == Verdict::Contradicted);
        if bad.len() * 2 > candidates.len() {
            run.warn(
                "the fact check disagreed with most of the answer; nothing was removed — treat this answer with care",
            );
            return (body.to_string(), vec![]);
        }
    }
    if bad.is_empty() {
        return (body.to_string(), vec![]);
    }
    let mut out = body.to_string();
    let mut sorted = bad.clone();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.0.start));
    for (u, _, _) in &sorted {
        out.replace_range(u.start..u.end, "");
    }
    (tidy(&out), bad.into_iter().map(|b| b.1).collect())
}

/// Clean up after removals: empty list items, doubled spaces, empty paragraphs.
fn tidy(md: &str) -> String {
    let mut lines: Vec<String> = vec![];
    for line in md.lines() {
        let t = line.trim();
        if t == "-"
            || t == "*"
            || (t.ends_with('.') && t.len() <= 3 && t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        {
            continue;
        }
        let mut l = line.trim_end().to_string();
        while l.contains("  ") && !l.trim_start().starts_with("    ") {
            l = l.replace("  ", " ");
        }
        lines.push(l);
    }
    let mut out = String::new();
    let mut blank = 0;
    for l in lines {
        if l.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(&l);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_names() {
        assert_eq!(
            names("In the series, Orla Brady plays Friday, who is based on the real-life person Edith Marks [1]."),
            vec!["orla brady", "friday", "edith marks"]
        );
        assert_eq!(names("F.R.I.D.A.Y. appears as Alison Hogan."), vec!["friday", "alison hogan"]);
        assert_eq!(names("Samuel L. Jackson stares."), vec!["samuel l jackson"]);
        assert_eq!(names("The show is good. It airs weekly."), Vec::<String>::new());
        assert_eq!(names("Friday is a character in Marvel's VisionQuest."), vec!["friday", "marvel", "visionquest"]);
        assert_eq!(
            names("between Allison Hogan and Happy Hogan (Tony’s longtime bodyguard)"),
            vec!["allison hogan", "happy hogan", "tony"]
        );
        assert_eq!(names("all of Stark’s AIs—except DUM-E and U—are modeled"), vec!["stark", "dum-e"]);
    }

    #[test]
    fn splits_statements() {
        let md = "# Title\n\nOrla Brady plays F.R.I.D.A.Y. in the show [1]. Edith Marks inspired E.D.I.T.H. [2]\n\n\
                  - First item with Tony Stark [3]\n- Second item\n\n```\ncode. Not split.\n```\n";
        let u: Vec<&str> = units(md).iter().map(|u| &md[u.start..u.end]).collect();
        assert_eq!(
            u,
            vec![
                "Orla Brady plays F.R.I.D.A.Y. in the show [1].",
                "Edith Marks inspired E.D.I.T.H. [2]",
                "- First item with Tony Stark [3]",
                "- Second item"
            ]
        );
    }

    #[test]
    fn passage_holds_the_deciding_sentence() {
        // the real source text behind a crossed-up answer
        let ev = vec![Evidence {
            label: "[5] VisionQuest episodes 3 & 4 explained".into(),
            text: "The episodes follow Vision as he resists Ultron. F.R.I.D.A.Y. appears as Alison Hogan, while \
                   E.D.I.T.H. takes the form of Edith Marks, a test pilot. Emily Hampshire plays E.D.I.T.H. in the show."
                .into(),
        }];
        let src = source_sentences(&ev);
        let p = passage(&src, &names("Friday is an AI avatar based on Edith Marks, played by Emily Hampshire"));
        assert!(
            p.contains("F.R.I.D.A.Y. appears as Alison Hogan, while E.D.I.T.H. takes the form of Edith Marks"),
            "{p}"
        );
        assert!(p.contains("Emily Hampshire plays E.D.I.T.H."), "{p}");
        // a name the sources never mention
        assert_eq!(missing(&src, &names("Friday is played by Scarlett Johansson")), vec!["scarlett johansson"]);
    }

    #[test]
    fn tidies_removals() {
        assert_eq!(tidy("A [1].  C.\n\n- \n- kept\n\n\n\nEnd"), "A [1]. C.\n\n- kept\n\nEnd");
    }
}
