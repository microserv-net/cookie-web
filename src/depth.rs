//! "Severity" triage: decide how much research a request deserves.
//!
//! `VisionQuest` → quick (a short answer + links).
//! `how do I set up wireguard on ubuntu` → standard.
//! `write a detailed report comparing open-weight LLMs for coding` → deep.
//!
//! Strategy: cheap heuristics first; if they are not confident, ask the
//! (fast) model for a structured classification; fall back to heuristics.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::llm::{ChatModel, ChatRequest, Message, Tier, parse_json_loose};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Depth {
    Quick,
    #[default]
    Standard,
    Deep,
}

impl Depth {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "quick" | "brief" | "short" | "1" => Some(Self::Quick),
            "standard" | "normal" | "medium" | "2" => Some(Self::Standard),
            "deep" | "report" | "research" | "long" | "3" => Some(Self::Deep),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Standard => "standard",
            Self::Deep => "deep",
        }
    }
    pub fn profile(&self) -> Profile {
        match self {
            Self::Quick => Profile {
                max_steps: 3,
                max_fetches: 1,
                results_per_search: 6,
                page_chars: 4_000,
                evidence_chars: 9_000,
                write_tokens: 600,
                gather_secs: 120,
                target: "A direct answer in one or two short paragraphs (roughly 60-180 words). \
                         Then a `**Links**` list with the 3-5 most useful links as `- [Title](url) — why it's useful`. \
                         No other headings.",
            },
            Self::Standard => Profile {
                max_steps: 6,
                max_fetches: 4,
                results_per_search: 8,
                page_chars: 6_000,
                evidence_chars: 22_000,
                write_tokens: 1_500,
                gather_secs: 300,
                target: "A well-structured answer of roughly 300-700 words: start with a one-paragraph direct answer, \
                         then a few `##` sections with bullets where they help. Inline citations throughout.",
            },
            Self::Deep => Profile {
                max_steps: 10,
                max_fetches: 10,
                results_per_search: 8,
                page_chars: 8_000,
                evidence_chars: 40_000,
                write_tokens: 3_500,
                gather_secs: 900,
                target: "A full research report of roughly 1000-2000 words. Start with `# <Title>`, then a \
                         `## TL;DR` of 3-5 bullets, then thematic `##` sections, a comparison table if \
                         things are being compared, and a final `## Caveats & open questions` section. \
                         Inline citations on every factual claim.",
            },
        }
    }
}

/// Rough characters-per-token for web text (conservative, so prompts don't overflow).
pub const CHARS_PER_TOKEN: f64 = 3.2;

impl Profile {
    /// Shrink budgets so prompt + answer fit the model's context window (`num_ctx`).
    /// Ollama silently drops the *start* of an over-long prompt (the instructions!), so this matters.
    pub fn fit_to_context(mut self, num_ctx: u32) -> Self {
        self.write_tokens = self.write_tokens.min(num_ctx / 2);
        let room = num_ctx.saturating_sub(self.write_tokens + 1_000) as f64 * CHARS_PER_TOKEN;
        self.evidence_chars = self.evidence_chars.min(room as usize).max(3_000);
        self.page_chars = self.page_chars.min(self.evidence_chars / 2).max(2_000);
        self
    }
}

/// Budgets and output targets for a depth level.
#[derive(Clone, Debug)]
pub struct Profile {
    /// Max agent-loop iterations (model turns that may call tools).
    pub max_steps: usize,
    /// Max full-page reads.
    pub max_fetches: usize,
    pub results_per_search: usize,
    /// Max characters of a page handed to the model.
    pub page_chars: usize,
    /// Max characters of evidence given to the writer.
    pub evidence_chars: usize,
    pub write_tokens: u32,
    /// Soft time budget for gathering (seconds, before `COOKIE_TIME_SCALE`); when it runs out,
    /// cookie-web stops researching and writes with what it has.
    pub gather_secs: u64,
    /// Formatting brief for the writer.
    pub target: &'static str,
}

/// What kind of request this is; shapes the output format.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// A name/term/thing: "VisionQuest", "who is Ada Lovelace".
    Entity,
    /// A specific factual question.
    #[default]
    Fact,
    /// Current events / latest news.
    News,
    /// Instructions: "how to …".
    Howto,
    /// A vs B.
    Compare,
    /// Open-ended research / analysis.
    Research,
    /// The user wants links/resources/downloads.
    Links,
}

impl Intent {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "entity" => Self::Entity,
            "news" => Self::News,
            "howto" | "how_to" | "how-to" => Self::Howto,
            "compare" | "comparison" => Self::Compare,
            "research" => Self::Research,
            "links" | "resources" => Self::Links,
            _ => Self::Fact,
        }
    }
    /// Extra formatting guidance for the writer.
    pub fn guidance(&self) -> &'static str {
        match self {
            Self::Entity => {
                "Say clearly what the term most likely refers to. If it is ambiguous, list the main meanings briefly."
            }
            Self::Fact => "Lead with the exact answer, then the supporting context.",
            Self::News => {
                "Lead with the latest developments, always with dates. Prefer the most recent sources and say how recent they are."
            }
            Self::Howto => {
                "Give numbered, copy-pasteable steps (use fenced code blocks for commands). Mention prerequisites and common pitfalls."
            }
            Self::Compare => {
                "Include a Markdown comparison table of the key dimensions, then a short recommendation by use-case."
            }
            Self::Research => {
                "Synthesise across sources, note where they agree or disagree, and quantify where possible."
            }
            Self::Links => {
                "Lead with a curated list of the best links, each as `- [Title](url) — one line on what it is`. Prefer official/primary sources."
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Triage {
    pub depth: Depth,
    pub intent: Intent,
    /// A search-engine-ready version of the request.
    pub search_query: String,
    pub reason: String,
    /// "user", "heuristic" or "model".
    pub decided_by: String,
}

/// Unmistakable requests for a long answer.
const DEEP_MARKERS: &[&str] = &[
    "report",
    "in-depth",
    "in depth",
    "deep dive",
    "deep-dive",
    "comprehensive",
    "thorough",
    "detailed",
    "literature review",
    "state of the art",
    "state-of-the-art",
    "everything about",
    "white paper",
    "whitepaper",
    "explain in detail",
    "write up",
    "write-up",
];
/// Words that *often* mean "go deep" but also appear in ordinary questions
/// ("who funds cancer research?"): they lean deep, but the model gets the final say.
const DEEP_HINTS: &[&str] = &[
    "research",
    "analysis",
    "analyse",
    "analyze",
    "landscape",
    "pros and cons",
    "market",
    "survey of",
    "overview of",
    "literature",
];
const COMPARE_MARKERS: &[&str] = &[
    " vs ",
    " vs. ",
    " versus ",
    "compare",
    "compared",
    "comparing",
    "comparison",
    "difference between",
    "better than",
    "alternatives to",
];
const NEWS_MARKERS: &[&str] =
    &["news", "latest", "today", "this week", "recent", "announced", "just released", "update on", "current status"];
const HOWTO_MARKERS: &[&str] = &[
    "how to",
    "how do i",
    "how can i",
    "how should i",
    "steps to",
    "guide",
    "tutorial",
    "install",
    "installing",
    "guides",
    "set up",
    "setup",
    "configure",
    "fix ",
    "error",
    "errors",
];
const LINK_MARKERS: &[&str] = &[
    "link",
    "links",
    "download",
    "resources",
    "where can i find",
    "official site",
    "documentation for",
    "docs for",
    "pdf",
    "github repo",
];
const QUESTION_WORDS: &[&str] =
    &["what", "who", "when", "where", "which", "why", "how", "is", "are", "does", "do", "can", "should", "will", "did"];

/// Does `hay` contain any of the phrases as whole words? ("link" must not match "Linkin Park",
/// "error" must not match "terror".) Phrases starting/ending with a space keep their own boundary.
fn has_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| {
        let n = n.trim();
        hay.match_indices(n).any(|(i, _)| {
            let before = hay[..i].chars().next_back();
            let after = hay[i + n.len()..].chars().next();
            let edge = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
            edge(before) && edge(after)
        })
    })
}

/// Conversational openers that only hurt as search keywords.
const LEAD_INS: &[&str] = &[
    "what can you tell me about",
    "what do you know about",
    "can you tell me about",
    "could you tell me about",
    "tell me everything about",
    "tell me about",
    "i want to know about",
    "i'd like to know about",
    "i would like to know about",
    "give me information on",
    "give me information about",
    "give me info on",
    "give me info about",
    "find information about",
    "find information on",
    "search the web for",
    "search for",
    "look up",
    "find me",
    "please",
    "can you",
    "could you",
];

/// A search-engine-friendly version of a conversational request (used when the model doesn't
/// provide one): strips lead-ins like "what can you tell me about", "hint:" labels and the
/// trailing question mark, keeping names and quoted phrases intact.
pub fn search_keywords(query: &str) -> String {
    let mut q = query.trim().to_string();
    loop {
        let low = q.to_lowercase();
        let Some(lead) = LEAD_INS
            .iter()
            .find(|l| low.starts_with(*l) && low[l.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric()))
        else {
            break;
        };
        q = q[lead.len()..].trim_start_matches([' ', ',', ':']).to_string();
    }
    let q = q.replace("hint:", " ").replace("Hint:", " ").replace("HINT:", " ");
    let q: String = q.split_whitespace().collect::<Vec<_>>().join(" ");
    let q = q.trim_end_matches(['?', '.', '!', ' ']).replace(" ,", ",");
    if q.trim().is_empty() { query.trim().trim_end_matches('?').to_string() } else { q }
}

/// Explicit user overrides: a leading `!quick`, `!standard` or `!deep` token.
pub fn split_override(query: &str) -> (Option<Depth>, String) {
    let q = query.trim();
    if let Some(rest) = q.strip_prefix('!') {
        let (word, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if let Some(d) = Depth::parse(word) {
            return (Some(d), tail.trim().to_string());
        }
    }
    (None, q.to_string())
}

/// Fast rule-based triage. Returns `(triage, confident)`.
pub fn heuristic(query: &str) -> (Triage, bool) {
    let q = format!(" {} ", query.trim().to_lowercase());
    let words: Vec<&str> = query.split_whitespace().collect();
    let n = words.len();
    let first = words.first().map(|w| w.to_lowercase()).unwrap_or_default();
    let is_question = query.contains('?') || QUESTION_WORDS.contains(&first.as_str());

    let intent = if has_any(&q, COMPARE_MARKERS) {
        Intent::Compare
    } else if has_any(&q, LINK_MARKERS) {
        Intent::Links
    } else if has_any(&q, HOWTO_MARKERS) {
        Intent::Howto
    } else if has_any(&q, NEWS_MARKERS) {
        Intent::News
    } else if has_any(&q, DEEP_MARKERS) || has_any(&q, DEEP_HINTS) {
        Intent::Research
    } else if n <= 4 && !is_question {
        Intent::Entity
    } else {
        Intent::Fact
    };

    let mk = |depth: Depth, reason: &str| Triage {
        depth,
        intent,
        search_query: search_keywords(query),
        reason: reason.to_string(),
        decided_by: "heuristic".into(),
    };

    if has_any(&q, DEEP_MARKERS) {
        return (mk(Depth::Deep, "asks for an in-depth report/analysis"), true);
    }
    if has_any(&q, DEEP_HINTS) || n >= 25 {
        return (mk(Depth::Deep, "looks like it wants a thorough answer"), false);
    }
    if n <= 3 && !is_question {
        return (mk(Depth::Quick, "a bare word/phrase: identify it and point to the best links"), true);
    }
    if intent == Intent::Compare {
        return (mk(Depth::Standard, "a comparison needs several sources"), n >= 4);
    }
    if is_question && n <= 8 && matches!(intent, Intent::Fact | Intent::Entity) {
        return (mk(Depth::Quick, "a short factual question"), false);
    }
    (mk(Depth::Standard, "a normal question"), false)
}

/// Full triage: override → confident heuristic → model → heuristic fallback.
pub async fn triage(llm: &dyn ChatModel, query: &str, today: &str) -> Triage {
    let (forced, clean) = split_override(query);
    let (mut h, confident) = heuristic(&clean);
    if let Some(d) = forced {
        h.depth = d;
        h.reason = "set explicitly by the user".into();
        h.decided_by = "user".into();
        return h;
    }
    if confident {
        return h;
    }

    let schema = json!({
        "type": "object",
        "properties": {
            "depth": { "type": "string", "enum": ["quick", "standard", "deep"] },
            "intent": { "type": "string", "enum": ["entity", "fact", "news", "howto", "compare", "research", "links"] },
            "search_query": { "type": "string" },
            "reason": { "type": "string" }
        },
        "required": ["depth", "intent", "search_query", "reason"]
    });
    let sys = format!(
        "You triage requests for a web research assistant. Today is {today}.\n\
         Decide how much research the request deserves:\n\
         - quick: a word, name, definition or a simple fact. Answer in a few sentences + links.\n\
         - standard: a normal question, how-to, or small comparison. About one page.\n\
         - deep: the user explicitly wants a report/analysis, or the topic needs many sources to answer well.\n\
         Never inflate: short inputs get short answers.\n\
         Also give the intent and a concise web search query (keywords, no quotes, keep names exact).\n\
         Reply with JSON only."
    );
    let req = ChatRequest::new(vec![Message::system(sys), Message::user(clean.clone())])
        .tier(Tier::Fast)
        .json(schema)
        .temperature(0.0)
        .max_tokens(200);
    match llm.chat(req).await.ok().and_then(|m| parse_json_loose(&m.content)) {
        Some(v) => {
            let depth = v["depth"].as_str().and_then(Depth::parse).unwrap_or(h.depth);
            let sq = v["search_query"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() < 300);
            Triage {
                depth,
                intent: v["intent"].as_str().map(Intent::parse).unwrap_or(h.intent),
                search_query: sq.map(String::from).unwrap_or(h.search_query),
                reason: v["reason"].as_str().unwrap_or("model decision").chars().take(240).collect(),
                decided_by: "model".into(),
            }
        }
        None => h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_word_is_quick() {
        let (t, c) = heuristic("VisionQuest");
        assert_eq!(t.depth, Depth::Quick);
        assert_eq!(t.intent, Intent::Entity);
        assert!(c);
    }

    #[test]
    fn report_is_deep() {
        let (t, c) = heuristic("write a detailed report on solid state batteries");
        assert_eq!(t.depth, Depth::Deep);
        assert!(c);
    }

    #[test]
    fn compare_is_standard() {
        let (t, _) = heuristic("rust vs go for web servers");
        assert_eq!(t.intent, Intent::Compare);
        assert_eq!(t.depth, Depth::Standard);
    }

    #[test]
    fn budgets_fit_small_context() {
        for d in [Depth::Quick, Depth::Standard, Depth::Deep] {
            let p = d.profile().fit_to_context(8192);
            let prompt_tokens = p.evidence_chars as f64 / CHARS_PER_TOKEN + 1_000.0;
            assert!(prompt_tokens + p.write_tokens as f64 <= 8192.0, "{d:?} overflows: {p:?}");
        }
        // a big window keeps the profile's own caps
        assert_eq!(Depth::Quick.profile().fit_to_context(65536).evidence_chars, 9_000);
    }

    #[test]
    fn markers_match_whole_words() {
        let (t, _) = heuristic("Linkin Park");
        assert_eq!(t.intent, Intent::Entity, "'link' must not match 'Linkin'");
        let (t, _) = heuristic("what is the terror bird");
        assert_ne!(t.intent, Intent::Howto, "'error' must not match 'terror'");
        let (t, _) = heuristic("download links for ubuntu 24.04");
        assert_eq!(t.intent, Intent::Links);
    }

    #[test]
    fn ambiguous_deep_words_defer_to_the_model() {
        let (t, confident) = heuristic("who funds most cancer research in europe");
        assert_eq!(t.depth, Depth::Deep);
        assert!(!confident);
        let (_, confident) = heuristic("give me a detailed report on cancer research funding");
        assert!(confident);
    }

    #[test]
    fn conversational_requests_become_keywords() {
        assert_eq!(
            search_keywords("what can you tell me about \"stares motherfuckingly\", hint: samuel l jackson"),
            "\"stares motherfuckingly\", samuel l jackson"
        );
        assert_eq!(search_keywords("Please look up the WireGuard docs?"), "the WireGuard docs");
        assert_eq!(search_keywords("who is Ada Lovelace?"), "who is Ada Lovelace");
        assert_eq!(search_keywords("please?"), "please");
    }

    #[test]
    fn override_prefix() {
        let (d, q) = split_override("!deep VisionQuest");
        assert_eq!(d, Some(Depth::Deep));
        assert_eq!(q, "VisionQuest");
        assert_eq!(split_override("!bang thing").0, None);
    }
}
