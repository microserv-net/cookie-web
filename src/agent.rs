//! Tool-calling agent loop used for `quick` and `standard` requests.
//!
//! Small models are unreliable at long agentic loops, so the loop is fenced:
//! the first search is always seeded by code (the system *always* uses its
//! tools), calls are budgeted and de-duplicated, and a deterministic floor
//! guarantees a minimum number of pages are actually read.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::{
    config::AgentMode,
    depth::{Depth, Intent, Profile, Triage},
    engine::{Evidence, Run},
    llm::{ChatRequest, Message, Tier, ToolCall, parse_json_loose},
    sources::{SourceKind, domain_of},
    tools::{
        ToolOutput,
        extract::{terms, truncate_chars},
        search::relax_query,
    },
};

/// Words that describe the *request* rather than its topic.
const REQUEST_WORDS: &[&str] = &[
    "tell",
    "know",
    "hint",
    "please",
    "info",
    "information",
    "explain",
    "find",
    "show",
    "give",
    "list",
    "anything",
    "something",
    "need",
    "want",
    "looking",
    "search",
    "wondering",
    "curious",
    "thanks",
    "help",
];

/// The distinctive words of a request, used to judge whether results are about it.
pub fn topic_terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    terms(query).into_iter().filter(|t| !REQUEST_WORDS.contains(&t.as_str()) && seen.insert(t.clone())).collect()
}

/// How many topic terms occur in `text` (lenient about word endings: "stares" matches "stare").
pub fn overlap(text: &str, topic: &[String]) -> usize {
    let low = text.to_lowercase();
    topic
        .iter()
        .filter(|t| {
            let n = t.chars().count();
            let keep = if n <= 4 { n } else { ((n as f64 * 0.75).round() as usize).max(4) };
            let stem: String = t.chars().take(keep).collect();
            low.contains(&stem)
        })
        .count()
}

/// Is this text plausibly about the request? (about a third of its distinctive terms appear)
pub fn relevant(text: &str, topic: &[String]) -> bool {
    topic.is_empty() || overlap(text, topic) >= ((topic.len() as f64 * 0.34).ceil() as usize).max(1)
}

/// Re-render a search result keeping only on-topic hits (with their citation numbers).
/// `None` if none of them is on topic.
fn on_topic_hits(run: &Run<'_>, out: &ToolOutput, topic: &[String]) -> Option<String> {
    let hits = out.data["hits"].as_array()?;
    let reg = run.ctx.sources.lock().unwrap();
    let mut text = String::new();
    let mut kept = 0;
    for h in hits {
        let (title, url, snippet) =
            (h["title"].as_str().unwrap_or(""), h["url"].as_str().unwrap_or(""), h["snippet"].as_str().unwrap_or(""));
        if !relevant(&format!("{title} {snippet} {url}"), topic) {
            continue;
        }
        let key = crate::sources::normalize_url(url);
        let Some(src) = reg.all().iter().find(|s| crate::sources::normalize_url(&s.url) == key) else { continue };
        kept += 1;
        text.push_str(&format!("[{}] {title} — {}\n    URL: {}\n", src.n, src.domain, src.url));
        if !snippet.is_empty() {
            text.push_str(&format!("    {}\n", snippet.chars().take(320).collect::<String>()));
        }
    }
    let header = out.text.lines().next().unwrap_or("Results:");
    (kept > 0).then(|| format!("{header} (off-topic results removed)\n{text}"))
}

/// Search hits in a tool result that look relevant to the request (0 for non-search tools or failures).
pub fn relevant_hits(ok: bool, out: &ToolOutput, topic: &[String]) -> usize {
    if !ok {
        return 0;
    }
    out.data["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|h| {
            let text = format!(
                "{} {} {}",
                h["title"].as_str().unwrap_or(""),
                h["snippet"].as_str().unwrap_or(""),
                h["url"].as_str().unwrap_or("")
            );
            relevant(&text, topic)
        })
        .count()
}

/// New search queries for a request whose searches found (almost) nothing.
/// Asks the fast model for rephrasings (synonyms, likely names, using any hints the user gave);
/// falls back to mechanical loosening if the model is unavailable.
pub async fn rephrase(run: &Run<'_>, tried: &[String]) -> Vec<String> {
    let schema = json!({
        "type": "object",
        "properties": { "queries": { "type": "array", "items": { "type": "string" } } },
        "required": ["queries"]
    });
    let sys = format!(
        "You help a web search that found nothing useful. Today is {}.\n\
         Write 3 different web search queries likely to find what the user wants: use synonyms and simpler keywords, \
         drop or replace unusual words, try the likely proper names or titles involved, and use any hints the user \
         gave. Keywords only, no quotes. Reply with JSON only.",
        run.today
    );
    let user =
        format!("Request: {}\nQueries already tried (avoid repeating them):\n- {}", run.query, tried.join("\n- "));
    let req = ChatRequest::new(vec![Message::system(sys), Message::user(user)])
        .tier(Tier::Fast)
        .json(schema)
        .temperature(0.5)
        .max_tokens(200);
    let mut out: Vec<String> = run
        .chat(req)
        .await
        .ok()
        .and_then(|m| parse_json_loose(&m.content))
        .map(|v| {
            v["queries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|q| q.as_str().map(|s| s.trim().trim_matches('"').to_string()))
                .filter(|q| !q.is_empty() && q.len() < 200)
                .collect()
        })
        .unwrap_or_default();
    // Mechanical fallbacks: loosened phrasing, then the bare keywords.
    let request = crate::depth::search_keywords(&run.query);
    for q in tried.iter().chain(std::iter::once(&request)) {
        if let Some(r) = relax_query(q) {
            out.push(r);
        }
        let kw = terms(q);
        if kw.len() >= 2 {
            out.push(kw.into_iter().take(6).collect::<Vec<_>>().join(" "));
        }
    }
    let mut seen: HashSet<String> = tried.iter().map(|q| q.to_lowercase()).collect();
    out.retain(|q| seen.insert(q.to_lowercase()));
    out.truncate(3);
    out
}

/// Domains that rarely yield readable text without JavaScript or a login.
pub const UNREADABLE: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "tiktok.com",
    "instagram.com",
    "facebook.com",
    "x.com",
    "twitter.com",
    "pinterest.com",
    "linkedin.com",
    "quora.com",
    // image/GIF hosts: nothing to read
    "giphy.com",
    "tenor.com",
    "imgur.com",
    "imgflip.com",
];

pub fn readable(url: &str) -> bool {
    let d = domain_of(url);
    !UNREADABLE.iter().any(|u| d == *u || d.ends_with(&format!(".{u}")))
}

fn seeds(t: &Triage, p: &Profile) -> Vec<(&'static str, Value)> {
    let q = t.search_query.clone();
    match t.intent {
        Intent::News => {
            vec![("news_search", json!({ "query": q, "time_range": "month" })), ("web_search", json!({ "query": q }))]
        }
        Intent::Entity => vec![("web_search", json!({ "query": q })), ("wikipedia", json!({ "topic": q }))],
        Intent::Links => vec![("web_search", json!({ "query": q, "max_results": 10 }))],
        _ => vec![("web_search", json!({ "query": q, "max_results": p.results_per_search }))],
    }
}

fn agent_system(run: &Run<'_>, t: &Triage, p: &Profile, native: bool) -> String {
    let reading = match t.depth {
        Depth::Quick => {
            "This is a QUICK request: only read a page if the snippets are not enough to answer confidently."
                .to_string()
        }
        _ => format!(
            "Read (fetch_page with a `focus`) the {} most promising results — snippets alone are rarely enough.",
            p.max_fetches.min(3)
        ),
    };
    let protocol = if native {
        "Call tools to gather information. When you have enough, reply with the single word DONE and no tool call."
            .to_string()
    } else {
        let tools: Vec<String> = run
            .engine
            .tools
            .specs()
            .iter()
            .map(|s| {
                format!(
                    "- {}: {} Parameters: {}",
                    s["function"]["name"].as_str().unwrap_or(""),
                    s["function"]["description"].as_str().unwrap_or(""),
                    s["function"]["parameters"]["properties"]
                )
            })
            .collect();
        format!(
            "Available tools:\n{}\n\nReply ONLY with JSON: {{\"tool\": \"<tool name>\", \"args\": {{...}}}} to call one tool, \
             or {{\"tool\": \"done\", \"args\": {{}}}} when you have enough information.",
            tools.join("\n")
        )
    };
    format!(
        "You are the research planner of cookie-web, a web research assistant. Today is {today}.\n\
         Your job is to GATHER the information needed to answer the user's request. A separate step writes the answer, \
         so do not write the answer yourself.\n\n\
         Budget: at most {steps} tool rounds and {fetches} page reads.\n\
         Strategy:\n\
         - Initial search results are already provided below.\n\
         - {reading}\n\
         - If results are off-topic, ambiguous or missing something, search again with different, more specific keywords.\n\
         - Use news_search for recent events, wikipedia for definitions/people/places, extract_links or crawl_site to find specific resources on a site.\n\
         - Prefer official, primary and recent sources. Never call the same tool with the same arguments twice.\n\n\
         {protocol}",
        today = run.today,
        steps = p.max_steps,
        fetches = p.max_fetches,
    )
}

pub async fn gather(run: &Run<'_>, t: &Triage, p: &Profile) -> anyhow::Result<Vec<Evidence>> {
    let mut evidence: Vec<Evidence> = vec![];
    let mut done: HashSet<String> = HashSet::new();
    let model = run.model.as_deref();
    let native = run.engine.llm.supports_tools(model).await;

    // 1. Seed searches (always run — the system must use its tools).
    run.events.status("Searching the web");
    let mut seed_calls = seeds(t, p);
    let mut seed_results =
        futures::future::join_all(seed_calls.iter().map(|(name, args)| run.tool(name, args.clone()))).await;

    // 1b. Rescue: if the searches came back empty or off-topic (none of the request's distinctive
    // words in the results), rephrase and search again before the model ever sees them —
    // small models give up too easily, and search engines misread conversational queries.
    let topic = topic_terms(&format!("{} {}", run.query, t.search_query));
    // Wikipedia almost always returns *some* article; it only counts if it is about the request.
    let wiki_found = seed_calls.iter().zip(&seed_results).any(|((name, _), (ok, out))| {
        *name == "wikipedia" && *ok && !out.summary.contains("no article") && relevant(&out.text, &topic)
    });
    let hits: usize = seed_results.iter().map(|(ok, out)| relevant_hits(*ok, out, &topic)).sum();
    if hits < 2 && !wiki_found && !run.out_of_time() {
        run.events.status("Results look thin or off-topic — trying other phrasings");
        let alts = rephrase(run, std::slice::from_ref(&t.search_query)).await;
        let extra: Vec<(&'static str, Value)> =
            alts.into_iter().map(|q| ("web_search", json!({ "query": q }))).collect();
        let extra_results =
            futures::future::join_all(extra.iter().map(|(name, args)| run.tool(name, args.clone()))).await;
        let more: usize = extra_results.iter().map(|(ok, out)| relevant_hits(*ok, out, &topic)).sum();
        if hits + more == 0 {
            run.warn("the web searches found little that matches this request, even after rephrasing");
        }
        seed_calls.extend(extra);
        seed_results.extend(extra_results);
    }
    let mut messages =
        vec![Message::system(agent_system(run, t, p, native)), Message::user(format!("Request: {}", run.query))];
    let mut seed_dump = String::new();
    let mut seed_tool_msgs = vec![];
    // Once some search found on-topic results, drop the searches that found only off-topic ones:
    // they would cost context and tempt a small model into citing them.
    let any_relevant = seed_results.iter().any(|(ok, out)| relevant_hits(*ok, out, &topic) > 0);
    for ((name, args), (ok, mut out)) in seed_calls.iter().zip(seed_results) {
        done.insert(format!("{name}{args}"));
        let is_search = out.data["hits"].is_array();
        if any_relevant && is_search {
            match on_topic_hits(run, &out, &topic) {
                Some(text) => out.text = text,
                None => {
                    out.text = format!(
                        "(Results for {} were off-topic and have been set aside.)",
                        args["query"].as_str().unwrap_or("this search")
                    );
                    seed_tool_msgs.push(Message::tool(name, out.text.clone()));
                    seed_dump.push_str(&format!("Result of {name} {args}:\n{}\n\n", out.text));
                    continue;
                }
            }
        }
        if ok {
            evidence.push(Evidence {
                label: format!("{name}: {}", args["query"].as_str().or(args["topic"].as_str()).unwrap_or("")),
                text: out.text.clone(),
            });
        }
        seed_tool_msgs.push(Message::tool(name, truncate_chars(&out.text, p.page_chars)));
        seed_dump.push_str(&format!("Result of {name} {args}:\n{}\n\n", truncate_chars(&out.text, p.page_chars)));
    }
    if native {
        let mut call_msg = Message::assistant("");
        call_msg.tool_calls = seed_calls.iter().map(|(n, a)| ToolCall::new(n, a.clone())).collect();
        messages.push(call_msg);
        messages.extend(seed_tool_msgs);
    } else {
        messages.push(Message::user(seed_dump.clone()));
    }

    // 2. Model-driven rounds (skipped in pipeline mode).
    let mut use_native = native;
    let mut switched = false;
    // Everything gathered so far as plain text, used if we must switch to the JSON protocol mid-way.
    let mut all_dump = seed_dump.clone();
    let rounds = if run.engine.cfg.agent_mode == AgentMode::Pipeline { 0 } else { p.max_steps };
    // Prompt budget for the loop: context minus the reply, tool specs and a safety margin.
    let loop_budget =
        (run.engine.cfg.num_ctx.saturating_sub(700 + 1_100) as f64 * crate::depth::CHARS_PER_TOKEN) as usize;
    for step in 0..rounds {
        if run.ctx.fetches_left() == 0 && step > 0 && t.depth == Depth::Quick {
            break;
        }
        if run.out_of_time() {
            run.warn("the time budget for research ran out; answering with what was found");
            break;
        }
        compact(&mut messages, loop_budget);
        let calls: Vec<(String, Value)> = if use_native {
            let req =
                ChatRequest::new(messages.clone()).tools(run.engine.tools.specs()).max_tokens(600).temperature(0.2);
            match run.chat(req).await {
                Ok(msg) => {
                    let calls: Vec<(String, Value)> = msg
                        .tool_calls
                        .iter()
                        .take(4)
                        .map(|c| (c.function.name.clone(), normalize_args(&c.function.arguments)))
                        .collect();
                    if calls.is_empty() {
                        break;
                    }
                    let mut m = Message::assistant(msg.content.clone());
                    m.tool_calls = calls.iter().map(|(n, a)| ToolCall::new(n, a.clone())).collect();
                    messages.push(m);
                    calls
                }
                Err(e) if !switched => {
                    // Small models sometimes emit malformed tool calls; the JSON protocol is more forgiving.
                    tracing::warn!("native tool calling failed ({e:#}); switching to the JSON protocol");
                    use_native = false;
                    switched = true;
                    messages = vec![
                        Message::system(agent_system(run, t, p, false)),
                        Message::user(format!("Request: {}", run.query)),
                        Message::user(format!(
                            "{all_dump}Next tool call as JSON, or {{\"tool\":\"done\",\"args\":{{}}}}."
                        )),
                    ];
                    continue;
                }
                Err(e) => {
                    tracing::warn!("agent step failed: {e:#}");
                    break;
                }
            }
        } else {
            let schema = json!({
                "type": "object",
                "properties": { "tool": { "type": "string" }, "args": { "type": "object" } },
                "required": ["tool", "args"]
            });
            let req = ChatRequest::new(messages.clone()).json(schema).max_tokens(300).temperature(0.1);
            let Ok(msg) = run.chat(req).await else { break };
            let Some(v) = parse_json_loose(&msg.content) else { break };
            let tool = v["tool"].as_str().unwrap_or("done").to_string();
            if tool.eq_ignore_ascii_case("done") || tool.is_empty() {
                break;
            }
            messages.push(Message::assistant(msg.content.clone()));
            vec![(tool, normalize_args(&v["args"]))]
        };

        run.events.status(format!("Research round {}", step + 1));
        let mut pending = vec![];
        for (name, args) in &calls {
            let key = format!("{name}{args}");
            if !done.insert(key) {
                pending.push((name.clone(), args.clone(), None));
            } else {
                pending.push((name.clone(), args.clone(), Some(())));
            }
        }
        let results = futures::future::join_all(pending.iter().map(|(name, args, fresh)| async move {
            match fresh {
                Some(()) => run.tool(name, args.clone()).await,
                None => (
                    false,
                    crate::tools::ToolOutput::new(
                        "You already ran this exact call; its result is above. Try something different or reply DONE.",
                        "duplicate",
                    ),
                ),
            }
        }))
        .await;
        let mut dump = String::new();
        for ((name, args, _), (ok, out)) in pending.iter().zip(results) {
            if ok {
                let label = args["url"].as_str().or(args["query"].as_str()).or(args["topic"].as_str()).unwrap_or("");
                evidence.push(Evidence { label: format!("{name}: {label}"), text: out.text.clone() });
            }
            all_dump.push_str(&format!("Result of {name}:\n{}\n\n", truncate_chars(&out.text, 1_500)));
            if use_native {
                messages.push(Message::tool(name, truncate_chars(&out.text, p.page_chars)));
            } else {
                dump.push_str(&format!("Result of {name}:\n{}\n\n", truncate_chars(&out.text, p.page_chars)));
            }
        }
        if !use_native {
            messages
                .push(Message::user(format!("{dump}Next tool call as JSON, or {{\"tool\":\"done\",\"args\":{{}}}}.")));
        }
    }

    // 3. Deterministic floor: make sure some pages were actually read.
    let want = match (t.depth, t.intent) {
        // a Wikipedia summary + snippets is enough to identify a name; otherwise read one page
        (Depth::Quick, Intent::Entity | Intent::Links) if wiki_found => 0,
        (Depth::Quick, _) => 1,
        _ => p.max_fetches.min(2),
    };
    let read = run.ctx.sources.lock().unwrap().pages_read();
    if read < want {
        let topic = topic_terms(&run.query);
        let candidates: Vec<String> = {
            let reg = run.ctx.sources.lock().unwrap();
            let mut c: Vec<(usize, &crate::sources::Source)> = reg
                .all()
                .iter()
                .filter(|s| !s.read && matches!(s.kind, SourceKind::Search | SourceKind::News) && readable(&s.url))
                .map(|s| (overlap(&format!("{} {} {}", s.title, s.snippet, s.url), &topic), s))
                .collect();
            // most on-topic first; search rank breaks ties (the sort is stable)
            c.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            c.into_iter().map(|(_, s)| s.url.clone()).take((want - read) + 4).collect()
        };
        if !candidates.is_empty() {
            run.events.status("Reading the top results");
        }
        let mut got = read;
        for url in candidates {
            if got >= want || run.ctx.fetches_left() == 0 {
                break;
            }
            let (ok, out) = run.tool("fetch_page", json!({ "url": url, "focus": run.query })).await;
            if ok && !out.summary.contains("budget") {
                evidence.push(Evidence { label: format!("fetch_page: {url}"), text: out.text });
                got += 1;
            }
        }
        if got < want {
            run.warn(format!("could only read {got} of the {want} pages wanted; the answer leans on search snippets"));
        }
    }
    Ok(evidence)
}

/// Keep the conversation inside the context window: shorten the oldest tool results first
/// (system prompt, request and the newest result are kept intact).
pub fn compact(messages: &mut [Message], budget_chars: usize) {
    let total = |m: &[Message]| m.iter().map(|x| x.content.len() + 40).sum::<usize>();
    if total(messages) <= budget_chars {
        return;
    }
    let last = messages.len().saturating_sub(1);
    for keep in [1_200usize, 400] {
        for i in 2..last {
            let m = &mut messages[i];
            if (m.role == "tool" || m.role == "user") && m.content.len() > keep + 100 {
                m.content = format!("{}\n…[older result shortened to save context]", truncate_chars(&m.content, keep));
            }
            if total(messages) <= budget_chars {
                return;
            }
        }
    }
}

/// Models sometimes send arguments as a JSON string; normalise to an object.
fn normalize_args(v: &Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({ "query": s })),
        Value::Null => json!({}),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_normalised() {
        assert_eq!(normalize_args(&json!("{\"url\":\"https://a\"}"))["url"], "https://a");
        assert_eq!(normalize_args(&json!("plain"))["query"], "plain");
    }

    #[test]
    fn compaction_shrinks_oldest_first() {
        let mut m = vec![
            Message::system("sys"),
            Message::user("Request: x"),
            Message::tool("web_search", "a".repeat(5000)),
            Message::tool("fetch_page", "b".repeat(5000)),
            Message::tool("fetch_page", "c".repeat(5000)),
        ];
        compact(&mut m, 9_000);
        let total: usize = m.iter().map(|x| x.content.len() + 40).sum();
        assert!(total <= 9_000, "{total}");
        assert!(m[2].content.len() < 1_500);
        assert_eq!(m[4].content.len(), 5000, "newest result kept intact");
        assert_eq!(m[0].content, "sys");
    }

    #[test]
    fn judges_relevance_by_distinctive_words() {
        let topic = topic_terms("what can you tell me about \"stares motherfuckingly\", hint: samuel l jackson");
        assert_eq!(topic, vec!["stares", "motherfuckingly", "samuel", "jackson"]);
        assert!(!relevant("Canva: Visual Suite for Everyone — design anything", &topic));
        assert!(!relevant("CAN | meaning in the Cambridge English Dictionary", &topic));
        assert!(relevant("Samuel L. Jackson's legendary stare in Pulp Fiction", &topic));
    }

    #[test]
    fn unreadable_domains() {
        assert!(!readable("https://www.youtube.com/watch?v=1"));
        assert!(readable("https://docs.rs/tokio"));
    }
}
