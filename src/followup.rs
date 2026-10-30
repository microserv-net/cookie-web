//! Follow-ups: ask more about an earlier report, or about one of its sources, without starting over.
//!
//! A follow-up reuses the parent report's sources *with their citation numbers*, so "[3]" means the
//! same page in both answers. It reads the full text of the relevant sources (stored with the report
//! when it was written, or fetched now and stored), answers from them, and searches the web only
//! when they don't cover the question.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use futures::{StreamExt, stream};
use serde_json::json;

use crate::{
    agent::{overlap, topic_terms},
    depth::{self, Depth},
    engine::{Counters, Engine, Evidence, ResearchRequest, ResearchResult, Run},
    events::{Event, Events},
    llm::{ChatRequest, Message, Tier, parse_json_loose},
    report,
    sources::{SourceRegistry, normalize_url},
    tools::{
        ToolCtx,
        extract::{Page, focus_excerpt, for_model},
    },
};

/// Most sources a follow-up reads when none are named.
const MAX_SOURCES: usize = 6;

/// When a follow-up may search the web for more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebMode {
    /// Only if the report's sources don't cover the question (never when sources are named).
    Auto,
    Never,
    Always,
}

impl WebMode {
    pub fn parse(s: Option<&str>) -> anyhow::Result<Self> {
        match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("auto") => Ok(Self::Auto),
            Some("never") | Some("no") | Some("false") => Ok(Self::Never),
            Some("always") | Some("yes") | Some("true") => Ok(Self::Always),
            Some(other) => bail!("invalid web mode '{other}': use auto, never or always"),
        }
    }
}

/// `@3`, `@3,5` or `@3 @5` in a question → source numbers, plus the question without them.
pub fn mentions(q: &str) -> (Vec<usize>, String) {
    static AT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"@(\d{1,3}(?:\s*,\s*\d{1,3})*)\b").unwrap());
    let mut nums = vec![];
    for c in AT.captures_iter(q) {
        for n in c[1].split(',').filter_map(|x| x.trim().parse::<usize>().ok()) {
            if !nums.contains(&n) {
                nums.push(n);
            }
        }
    }
    let rest = AT.replace_all(q, |c: &regex::Captures| {
        // "what does @3 say" reads better to the model as "what does source [3] say"
        let ns: Vec<String> = c[1].split(',').map(|x| format!("[{}]", x.trim())).collect();
        format!("source {}", ns.join(""))
    });
    (nums, rest.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// The full page behind source `n` of a report: the copy stored with the report, or read now
/// (and stored, so the next request is instant).
pub async fn source_page(engine: &Engine, report: &ResearchResult, n: usize) -> anyhow::Result<Arc<Page>> {
    let src = report.sources.iter().find(|s| s.n == n).ok_or_else(|| anyhow!("report has no source [{n}]"))?;
    if let Some(p) = engine.store.pages(&report.id).await.remove(&n) {
        return Ok(Arc::new(p));
    }
    let web = engine.tools.web.as_ref().ok_or_else(|| anyhow!("reading pages is not available in this setup"))?;
    let page = web.page(&src.url).await?;
    if let Err(e) = engine.store.add_page(&report.id, n, &page).await {
        tracing::warn!("could not store page [{n}] of {}: {e:#}", report.id);
    }
    Ok(page)
}

/// Turn a context-dependent follow-up ("how old is he?") into a standalone search query.
async fn standalone_query(run: &Run<'_>, earlier: &str, question: &str) -> String {
    let schema = json!({
        "type": "object", "properties": { "query": { "type": "string" } }, "required": ["query"]
    });
    let sys = "Rewrite a follow-up question as one standalone web search query (keywords, no quotes), using the \
               earlier request for missing context such as names. Reply with JSON only.";
    let user = format!("Earlier request: {earlier}\nFollow-up question: {question}");
    let req = ChatRequest::new(vec![Message::system(sys), Message::user(user)])
        .tier(Tier::Fast)
        .json(schema)
        .temperature(0.1)
        .max_tokens(80);
    if let Some(q) = run
        .chat(req)
        .await
        .ok()
        .and_then(|m| parse_json_loose(&m.content))
        .and_then(|v| v["query"].as_str().map(|s| s.trim().to_string()))
        .filter(|q| !q.is_empty() && q.len() < 300)
    {
        return q;
    }
    // without the model: the earlier topic's keywords + the question's
    let mut words: Vec<String> = topic_terms(&depth::search_keywords(earlier));
    for w in topic_terms(question) {
        if !words.contains(&w) {
            words.push(w);
        }
    }
    words.truncate(10);
    words.join(" ")
}

/// Answer `req` (which has `report` set) as a follow-up on that report.
pub async fn run(
    engine: &Engine,
    req: ResearchRequest,
    events: Events,
    started: Instant,
    created_at: DateTime<Utc>,
    today: String,
    control: std::sync::Arc<crate::control::Control>,
) -> anyhow::Result<ResearchResult> {
    let report_id = req.report.clone().unwrap_or_default();
    let parent = engine
        .store
        .get(&report_id)
        .await
        .ok_or_else(|| anyhow!("no report '{report_id}' (it was never saved, or has been deleted)"))?;
    let web_mode = WebMode::parse(req.web.as_deref())?;
    let (forced, query) = depth::split_override(&req.query);
    let (mentioned, question) = mentions(&query);
    if question.trim().is_empty() {
        bail!("the follow-up question is empty");
    }

    // Which of the parent's sources to use.
    let mut targets: Vec<usize> = req.sources.clone().unwrap_or_default();
    for n in mentioned {
        if !targets.contains(&n) {
            targets.push(n);
        }
    }
    for n in &targets {
        if !parent.sources.iter().any(|s| s.n == *n) {
            bail!("report {report_id} has no source [{n}] (it has {})", parent.sources.len());
        }
    }
    let named = !targets.is_empty();
    if !named {
        targets = parent.sources.iter().filter(|s| s.cited || s.read).map(|s| s.n).take(MAX_SOURCES).collect();
        if targets.is_empty() {
            targets = parent.sources.iter().map(|s| s.n).take(4).collect();
        }
    }

    // Triage: follow-ups are usually short; a forced depth (field or !prefix) wins.
    let forced = req.depth.as_deref().filter(|d| !d.eq_ignore_ascii_case("auto")).and_then(Depth::parse).or(forced);
    if req.depth.as_deref().is_some_and(|d| !d.eq_ignore_ascii_case("auto")) && forced.is_none() {
        bail!("invalid depth '{}': use auto, quick, standard or deep", req.depth.clone().unwrap_or_default());
    }
    let (mut triage, _) = depth::heuristic(&question);
    triage.depth = forced.unwrap_or(match triage.depth {
        Depth::Deep => Depth::Standard,
        d => d,
    });
    triage.reason = if named {
        format!(
            "follow-up on source{} {} of the earlier report",
            if targets.len() > 1 { "s" } else { "" },
            cites(&targets)
        )
    } else {
        "follow-up on the earlier report".into()
    };
    triage.decided_by = if forced.is_some() { "user".into() } else { "follow-up".into() };
    let profile = triage.depth.profile().limit_words(req.max_words).fit_to_context(engine.cfg.num_ctx);
    let gather = Duration::from_secs_f64(profile.gather_secs as f64 * engine.cfg.time_scale)
        .min(engine.cfg.job_timeout.mul_f64(0.6));

    let ctx = ToolCtx::new(events.clone(), profile.results_per_search, profile.page_chars, profile.max_fetches)
        .with_sources(SourceRegistry::from_sources(&parent.sources));
    let run = Run {
        engine,
        ctx,
        events: events.clone(),
        counters: Counters::default(),
        model: req.model.clone(),
        today,
        query: question.clone(),
        deadline: started + gather,
        warnings: Mutex::new(vec![]),
        pages: Mutex::new(HashMap::new()),
        context: Some(format!(
            "This is a follow-up to an earlier request: \"{}\". Source numbers are the same as in the earlier answer.",
            parent.query.replace('"', "'")
        )),
        title: Some(query.trim().to_string()),
        control,
    };
    if !named || web_mode == WebMode::Always {
        triage.search_query = standalone_query(&run, &parent.query, &question).await;
    }
    events.emit(Event::Triage(triage.clone()));

    // 1. The earlier answer, for context ("explain the second point").
    let mut evidence = vec![Evidence {
        label: format!("Earlier answer to \"{}\"", parent.query),
        text: crate::tools::extract::truncate_chars(&report::answer_body(&parent.markdown), 2_500),
    }];

    // 2. The full text of the sources in question, excerpted around the question.
    events.status(format!("Reading source{} {}", if targets.len() > 1 { "s" } else { "" }, cites(&targets)));
    let focus = format!("{question} {}", triage.search_query);
    let pages: Vec<(usize, anyhow::Result<Arc<Page>>)> = stream::iter(targets.clone())
        .map(|n| {
            let parent = &parent;
            async move { (n, source_page(engine, parent, n).await) }
        })
        .buffered(3)
        .collect()
        .await;
    let topic = topic_terms(&question);
    let mut covered = topic.is_empty(); // "tell me more" is about the report itself
    for (n, page) in pages {
        let src = parent.sources.iter().find(|s| s.n == n).expect("validated above");
        match page {
            Ok(p) => {
                let excerpt = focus_excerpt(&for_model(&p.markdown, false), Some(&focus), profile.page_chars);
                if !topic.is_empty() && overlap(&excerpt, &topic) * 2 >= topic.len() {
                    covered = true;
                }
                run.ctx.sources.lock().unwrap().mark_read(n);
                run.pages.lock().unwrap().insert(normalize_url(&src.url), p.clone());
                evidence.push(Evidence { label: format!("[{n}] {} — {}", p.title, src.url), text: excerpt });
            }
            Err(e) => {
                run.warn(format!("could not read source [{n}] ({e:#}); used its search snippet"));
                if !src.snippet.is_empty() {
                    evidence.push(Evidence {
                        label: format!("[{n}] {} — {}", src.title, src.url),
                        text: src.snippet.clone(),
                    });
                }
            }
        }
    }

    // 3. More from the web, when the sources don't cover it (or the caller asked for it).
    let search = match web_mode {
        WebMode::Never => false,
        WebMode::Always => true,
        WebMode::Auto => !named && !covered,
    };
    if search && !run.out_of_time() {
        events.status("The earlier sources don't cover this — searching the web");
        evidence.extend(crate::agent::gather(&run, &triage, &profile).await?);
    } else if web_mode == WebMode::Auto && named && !covered {
        run.warn(format!(
            "source{} {} may not cover this question; ask without naming sources (or with web: always) to search the web",
            if targets.len() > 1 { "s" } else { "" },
            cites(&targets)
        ));
    }
    if evidence.len() < 2 {
        bail!("none of the report's sources could be read, and the web was not searched");
    }

    engine.finish(&run, triage, &profile, question, &evidence, &req, started, created_at, Some(report_id)).await
}

fn cites(ns: &[usize]) -> String {
    let set: HashSet<usize> = ns.iter().copied().collect();
    let mut v: Vec<usize> = set.into_iter().collect();
    v.sort_unstable();
    v.iter().map(|n| format!("[{n}]")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mentions() {
        let (n, q) = mentions("what does @3 say about pricing, and @1,4?");
        assert_eq!(n, vec![3, 1, 4]);
        assert_eq!(q, "what does source [3] say about pricing, and source [1][4]?");
        let (n, q) = mentions("email me@example.com about it");
        assert!(n.is_empty(), "{n:?}");
        assert_eq!(q, "email me@example.com about it");
    }

    #[test]
    fn web_modes() {
        assert_eq!(WebMode::parse(None).unwrap(), WebMode::Auto);
        assert_eq!(WebMode::parse(Some("NEVER")).unwrap(), WebMode::Never);
        assert!(WebMode::parse(Some("sometimes")).is_err());
    }
}
