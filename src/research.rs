//! Deep research pipeline (used for `deep` requests).
//!
//! plan sub-questions & queries → parallel searches → rank & diversify URLs →
//! parallel page reads → per-source note-taking → gap check → follow-up round.
//! Structured steps keep a small model on rails while still covering a topic broadly.

use std::collections::{HashMap, HashSet};

use futures::{StreamExt, stream};
use serde_json::{Value, json};

use crate::{
    agent::{readable, relevant, rephrase, topic_terms},
    depth::{Intent, Profile, Triage},
    engine::{Evidence, Run},
    llm::{ChatRequest, Message, Tier, parse_json_loose},
    sources::{domain_of, normalize_url},
    tools::{extract::truncate_chars, search::Hit},
};

/// Page text handed to the note-taker. Kept modest: on CPU-only boxes prompt processing
/// dominates runtime, and focused excerpts already put the relevant passages first.
const NOTE_INPUT_CHARS: usize = 6_000;

struct Plan {
    sub_questions: Vec<String>,
    queries: Vec<String>,
}

fn strings(v: &Value, max: usize) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty() && s.len() < 200)
        .take(max)
        .collect()
}

async fn plan(run: &Run<'_>, t: &Triage) -> Plan {
    let schema = json!({
        "type": "object",
        "properties": {
            "sub_questions": { "type": "array", "items": { "type": "string" } },
            "queries": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["sub_questions", "queries"]
    });
    let sys = format!(
        "You plan web research. Today is {}.\nBreak the request into 3-6 sub-questions that together answer it fully, \
         and write 4-6 diverse web search queries (keywords, not sentences; include one for recent developments and \
         one for authoritative/official sources). Reply with JSON only.",
        run.today
    );
    let req = ChatRequest::new(vec![Message::system(sys), Message::user(run.query.clone())])
        .tier(Tier::Fast)
        .json(schema)
        .temperature(0.2)
        .max_tokens(500);
    let parsed = run.chat(req).await.ok().and_then(|m| parse_json_loose(&m.content));
    let mut p = Plan {
        sub_questions: parsed.as_ref().map(|v| strings(&v["sub_questions"], 6)).unwrap_or_default(),
        queries: parsed.as_ref().map(|v| strings(&v["queries"], 6)).unwrap_or_default(),
    };
    // always include the triaged query; dedupe
    p.queries.insert(0, t.search_query.clone());
    let mut seen = HashSet::new();
    p.queries.retain(|q| seen.insert(q.to_lowercase()));
    if p.queries.len() < 3 {
        let y = chrono::Utc::now().format("%Y").to_string();
        for extra in [format!("{} overview", t.search_query), format!("{} {y}", t.search_query)] {
            if seen.insert(extra.to_lowercase()) {
                p.queries.push(extra);
            }
        }
    }
    p.queries.truncate(6);
    p
}

/// Run searches, return hits ranked by reciprocal-rank fusion across queries.
async fn search_round(run: &Run<'_>, queries: &[String], news: bool) -> Vec<Hit> {
    let calls: Vec<(String, Value)> = queries
        .iter()
        .flat_map(|q| {
            let mut v = vec![("web_search".to_string(), json!({ "query": q }))];
            if news {
                v.push(("news_search".to_string(), json!({ "query": q, "time_range": "month" })));
            }
            v
        })
        .collect();
    let results: Vec<_> = stream::iter(calls)
        .map(|(name, args)| async move { run.tool(&name, args).await })
        .buffer_unordered(4)
        .collect()
        .await;
    let mut scores: HashMap<String, (f64, Hit)> = HashMap::new();
    for (ok, out) in results {
        if !ok {
            continue;
        }
        let hits: Vec<Hit> = serde_json::from_value(out.data["hits"].clone()).unwrap_or_default();
        for (rank, h) in hits.into_iter().enumerate() {
            let s = 1.0 / (60.0 + rank as f64 + 1.0);
            scores.entry(normalize_url(&h.url)).and_modify(|(sc, _)| *sc += s).or_insert((s, h));
        }
    }
    let mut v: Vec<(f64, Hit)> = scores.into_values().collect();
    v.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    v.into_iter().map(|(_, h)| h).collect()
}

/// Pick up to `n` readable, unseen URLs with at most 2 per domain.
fn choose(hits: &[Hit], n: usize, seen: &mut HashSet<String>) -> Vec<String> {
    let mut per_domain: HashMap<String, usize> = HashMap::new();
    let mut out = vec![];
    for h in hits {
        if out.len() >= n {
            break;
        }
        let key = normalize_url(&h.url);
        let d = domain_of(&h.url);
        if !readable(&h.url) || seen.contains(&key) || *per_domain.get(&d).unwrap_or(&0) >= 2 {
            continue;
        }
        *per_domain.entry(d).or_default() += 1;
        seen.insert(key);
        out.push(h.url.clone());
    }
    out
}

/// Read pages and distil each into notes. Returns the notes and how many pages were actually read.
async fn read_and_note(run: &Run<'_>, urls: Vec<String>, sub_questions: &[String]) -> (Vec<Evidence>, usize) {
    run.events.status(format!("Reading {} sources", urls.len()));
    let pages: Vec<_> = stream::iter(urls)
        .map(|u| async move { run.tool("fetch_page", json!({ "url": u, "focus": run.query })).await })
        .buffer_unordered(4)
        .collect()
        .await;
    let pages: Vec<String> =
        pages.into_iter().filter(|(ok, o)| *ok && !o.summary.contains("budget")).map(|(_, o)| o.text).collect();

    let subs = if sub_questions.is_empty() {
        String::new()
    } else {
        format!("\nSub-questions:\n- {}", sub_questions.join("\n- "))
    };
    let mut out = vec![];
    for (i, text) in pages.iter().enumerate() {
        let header = text.lines().next().unwrap_or("source").to_string();
        if run.out_of_time() {
            // No time left for the model to take notes: hand the writer a focused excerpt instead.
            out.push(Evidence { label: header, text: truncate_chars(text, 1500) });
            continue;
        }
        run.events.status(format!(
            "Taking notes {}/{}: {}",
            i + 1,
            pages.len(),
            header.chars().take(80).collect::<String>()
        ));
        let sys = "You extract research notes. From the source, list up to 8 concise bullet points of concrete facts \
                   relevant to the research question (keep numbers, dates, names, versions; note the source's own caveats). \
                   No intro, no opinions about the source. If nothing is relevant, reply exactly NONE.";
        let user =
            format!("Research question: {}{subs}\n\nSource:\n{}", run.query, truncate_chars(text, NOTE_INPUT_CHARS));
        let req = ChatRequest::new(vec![Message::system(sys), Message::user(user)])
            .tier(Tier::Fast)
            .max_tokens(450)
            .temperature(0.1);
        match run.chat(req).await {
            Ok(m) if m.content.trim().eq_ignore_ascii_case("none") => continue,
            Ok(m) if !m.content.trim().is_empty() => {
                out.push(Evidence { label: header, text: m.content.trim().to_string() })
            }
            // note-taking failed: fall back to a raw excerpt
            _ => out.push(Evidence { label: header, text: truncate_chars(text, 1500) }),
        }
    }
    (out, pages.len())
}

async fn gaps(run: &Run<'_>, sub_questions: &[String], notes: &[Evidence]) -> Vec<String> {
    let digest: String = notes.iter().map(|e| format!("{}\n{}\n", e.label, truncate_chars(&e.text, 600))).collect();
    let schema = json!({
        "type": "object",
        "properties": { "enough": { "type": "boolean" }, "follow_up_queries": { "type": "array", "items": { "type": "string" } } },
        "required": ["enough", "follow_up_queries"]
    });
    let sys = "You review research coverage. Given the question, its sub-questions and the notes collected so far, \
               decide whether the notes are enough for a thorough report. If not, give up to 3 web search queries \
               (keywords) that would fill the most important gaps. Reply with JSON only.";
    let user = format!(
        "Question: {}\nSub-questions:\n- {}\n\nNotes so far:\n{}",
        run.query,
        sub_questions.join("\n- "),
        truncate_chars(&digest, 6000)
    );
    let req = ChatRequest::new(vec![Message::system(sys), Message::user(user)])
        .tier(Tier::Fast)
        .json(schema)
        .max_tokens(250)
        .temperature(0.1);
    match run.chat(req).await.ok().and_then(|m| parse_json_loose(&m.content)) {
        Some(v) if v["enough"].as_bool() != Some(true) => strings(&v["follow_up_queries"], 3),
        _ => vec![],
    }
}

pub async fn gather(run: &Run<'_>, t: &Triage, p: &Profile) -> anyhow::Result<Vec<Evidence>> {
    run.events.status("Planning the research");
    let plan = plan(run, t).await;
    if !plan.sub_questions.is_empty() {
        run.events.status(format!("Sub-questions: {}", plan.sub_questions.join(" · ")));
    }
    let news = t.intent == Intent::News;
    let mut evidence = vec![];

    if matches!(t.intent, Intent::Entity | Intent::Research | Intent::Compare) {
        let (ok, out) = run.tool("wikipedia", json!({ "topic": t.search_query })).await;
        if ok && !out.summary.contains("no article") {
            evidence.push(Evidence { label: "Background (Wikipedia)".into(), text: out.text });
        }
    }

    run.events.status(format!("Running {} searches", plan.queries.len()));
    let mut hits = search_round(run, &plan.queries, news).await;
    let topic = topic_terms(&run.query);
    let on_topic = hits.iter().filter(|h| relevant(&format!("{} {} {}", h.title, h.snippet, h.url), &topic)).count();
    if on_topic < 3 {
        run.events.status("Few results — trying other phrasings");
        let alts = rephrase(run, &plan.queries).await;
        hits.extend(search_round(run, &alts, news).await);
        if hits.is_empty() {
            run.warn("the web searches found almost nothing for this request, even after rephrasing");
        }
    }
    let mut seen = HashSet::new();
    let first = p.max_fetches.saturating_sub(3).max(3);
    let urls = choose(&hits, first, &mut seen);
    let (notes, mut read) = read_and_note(run, urls, &plan.sub_questions).await;
    evidence.extend(notes);
    // Dead links and blocked pages: top up from the next-best results.
    if read < first.min(3) && run.ctx.fetches_left() > 0 && !run.out_of_time() {
        let n = (first - read).min(run.ctx.fetches_left() as usize);
        let urls = choose(&hits, n, &mut seen);
        if !urls.is_empty() {
            let (notes, more) = read_and_note(run, urls, &plan.sub_questions).await;
            evidence.extend(notes);
            read += more;
        }
    }
    if read == 0 {
        run.warn("no page could be read in full; the report relies on search snippets");
    }

    // Follow-up round for gaps (skipped when the time budget is spent).
    let follow = if run.out_of_time() {
        run.warn("the time budget for research ran out; skipped the gap-filling round");
        vec![]
    } else {
        gaps(run, &plan.sub_questions, &evidence).await
    };
    if !follow.is_empty() && run.ctx.fetches_left() > 0 {
        run.events.status(format!("Filling gaps: {}", follow.join(" · ")));
        let more = search_round(run, &follow, news).await;
        let n = (run.ctx.fetches_left() as usize).min(3);
        let urls = choose(&more, n, &mut seen);
        evidence.extend(read_and_note(run, urls, &plan.sub_questions).await.0);
    }

    // Snippets of results we did not read: cheap extra breadth for the writer.
    let snippets: String = {
        let reg = run.ctx.sources.lock().unwrap();
        reg.all()
            .iter()
            .filter(|s| !s.read && !s.snippet.is_empty())
            .take(12)
            .map(|s| format!("[{}] {} — {}\n", s.n, s.title, s.snippet))
            .collect()
    };
    if !snippets.is_empty() {
        evidence.push(Evidence { label: "Other search snippets (not read in full)".into(), text: snippets });
    }
    Ok(evidence)
}
