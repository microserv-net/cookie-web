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
    "her",
    "him",
    "his",
    "she",
    "he",
    "they",
    "them",
    "their",
    "its",
    "plays",
    "play",
    "played",
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

/// Is `text` about the request? Checked against each topic separately (the request's own words, the
/// search query's), so a search query that added or changed words can't make everything look off-topic.
/// `share` is the fraction of a topic's distinctive words that must appear (at least one).
pub fn about(text: &str, topics: &[&[String]], share: f64) -> bool {
    topics.iter().all(|t| t.is_empty())
        || topics
            .iter()
            .filter(|t| !t.is_empty())
            .any(|t| overlap(text, t) >= ((t.len() as f64 * share).ceil() as usize).max(1))
}

/// The two descriptions of what the request is about: its own words and the search query's.
pub fn topics_of(run: &Run<'_>, t: &Triage) -> (Vec<String>, Vec<String>) {
    (topic_terms(&run.query), topic_terms(&t.search_query))
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
            "This is a QUICK request: only read another page if what you have is not enough to answer confidently."
                .to_string()
        }
        _ => "Read more pages (fetch_page with a `focus`) only for parts of the request the pages below don't cover."
            .to_string(),
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
         - The search results and the most promising pages have already been read for you (below).\n\
         - First decide: is this enough to answer the request fully and correctly? If yes, stop now.\n\
         - If not, call the tools that fill the gaps. Call several at once when they don't depend on each other.\n\
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
        // ask every search engine this time: one engine misreading the query shouldn't sink it
        let extra: Vec<(&'static str, Value)> =
            alts.into_iter().map(|q| ("web_search", json!({ "query": q, "broad": true }))).collect();
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

    // 1c. Read the most promising pages right away, in parallel. Code can pick on-topic pages from the
    // snippets as well as the model can, and the model then judges with the pages in front of it —
    // usually saving it a whole round (each round re-reads the conversation, the slow part on a CPU).
    let want = match (t.depth, t.intent) {
        // a Wikipedia summary + snippets is enough to identify a name; otherwise read one page
        (Depth::Quick, Intent::Entity | Intent::Links) if wiki_found => 0,
        (Depth::Quick, _) => 1,
        _ => p.max_fetches.min(2),
    };
    let reads = read_top(run, t, want).await;
    if !reads.is_empty() {
        let mut call_msg = Message::assistant("");
        let mut dump = String::new();
        let mut tool_msgs = vec![];
        for ((name, args), (ok, out)) in &reads {
            done.insert(format!("{name}{args}"));
            call_msg.tool_calls.push(ToolCall::new(name, args.clone()));
            if *ok {
                let url = args["url"].as_str().unwrap_or("");
                evidence.push(Evidence { label: format!("fetch_page: {url}"), text: out.text.clone() });
            }
            // The planner only judges coverage, so it gets a shorter excerpt; the writer gets the full one.
            let short = truncate_chars(&out.text, p.page_chars.min(PLANNER_PAGE_CHARS));
            tool_msgs.push(Message::tool(name, short.clone()));
            dump.push_str(&format!("Result of {name} {args}:\n{short}\n\n"));
        }
        seed_dump.push_str(&dump);
        if native {
            messages.push(call_msg);
            messages.extend(tool_msgs);
        } else {
            messages.push(Message::user(dump));
        }
    }

    // 2. Model-driven rounds (skipped in pipeline mode).
    let mut use_native = native;
    let mut switched = false;
    // Everything gathered so far as plain text, used if we must switch to the JSON protocol mid-way.
    let mut all_dump = seed_dump.clone();
    let pages_read = run.ctx.sources.lock().unwrap().pages_read();
    // A quick request with an on-topic page or Wikipedia article in hand needs no model round at all.
    let quick_enough = t.depth == Depth::Quick && (wiki_found || pages_read > 0);
    let rounds = if run.engine.cfg.agent_mode == AgentMode::Pipeline || quick_enough { 0 } else { p.max_steps };
    if rounds > 0 {
        run.events.status("Deciding whether more research is needed");
    }
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

    // 3. Deterministic floor: if reads failed (dead links, bot walls), read the next on-topic results.
    let read = run.ctx.sources.lock().unwrap().pages_read();
    if read < want && !run.out_of_time() {
        for ((_, args), (ok, out)) in read_top(run, t, want - read).await {
            if ok {
                let url = args["url"].as_str().unwrap_or("");
                evidence.push(Evidence { label: format!("fetch_page: {url}"), text: out.text });
            }
        }
        let got = run.ctx.sources.lock().unwrap().pages_read();
        if got < want {
            run.warn(format!("could only read {got} of the {want} pages wanted; the answer leans on search snippets"));
        }
    }
    let (asked, searched) = topics_of(run, t);
    Ok(consolidate(run, &[&asked, &searched], evidence, p.results_per_search + p.results_per_search / 2))
}

/// How much of each pre-read page the planner sees (enough to judge what is covered).
const PLANNER_PAGE_CHARS: usize = 1_800;

/// Read up to `n` of the most on-topic unread search results, in parallel; when some fail (dead link,
/// bot wall), one more wave reads the next ones. Off-topic results are never read.
async fn read_top(run: &Run<'_>, t: &Triage, n: usize) -> Vec<((String, Value), (bool, crate::tools::ToolOutput))> {
    if n == 0 || run.ctx.fetches_left() == 0 {
        return vec![];
    }
    let (asked, searched) = topics_of(run, t);
    let topic = topic_terms(&format!("{} {}", run.query, t.search_query));
    let mut candidates: Vec<String> = {
        let reg = run.ctx.sources.lock().unwrap();
        let mut c: Vec<(usize, &crate::sources::Source)> = reg
            .all()
            .iter()
            .filter(|s| !s.read && matches!(s.kind, SourceKind::Search | SourceKind::News) && readable(&s.url))
            .filter(|s| about(&format!("{} {} {}", s.title, s.snippet, s.url), &[&asked, &searched], 0.25))
            .map(|s| (overlap(&format!("{} {} {}", s.title, s.snippet, s.url), &topic), s))
            .collect();
        // most on-topic first; search rank breaks ties (the sort is stable)
        c.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        c.into_iter().map(|(_, s)| s.url.clone()).take(n + 4).collect()
    };
    if candidates.is_empty() {
        return vec![];
    }
    run.events.status(format!("Reading the top {} result{}", n.min(candidates.len()), if n == 1 { "" } else { "s" }));
    let focus = focus_for(run, t);
    let mut out = vec![];
    let mut got = 0;
    for _wave in 0..2 {
        let take = (n - got).min(candidates.len()).min(run.ctx.fetches_left() as usize);
        if take == 0 {
            break;
        }
        let batch: Vec<(String, Value)> =
            candidates.drain(..take).map(|u| ("fetch_page".to_string(), json!({ "url": u, "focus": focus }))).collect();
        let results = futures::future::join_all(batch.iter().map(|(name, args)| run.tool(name, args.clone()))).await;
        for (call, (ok, o)) in batch.into_iter().zip(results) {
            let ok = ok && !o.summary.contains("budget");
            got += ok as usize;
            out.push((call, (ok, o)));
        }
        if got >= n {
            break;
        }
    }
    out
}

/// What to look for on a page: the request's keywords (not its conversational wording).
pub fn focus_for(run: &Run<'_>, t: &Triage) -> String {
    let kw = crate::depth::search_keywords(&run.query);
    if kw.to_lowercase().contains(&t.search_query.to_lowercase()) { kw } else { format!("{kw} {}", t.search_query) }
}

/// The evidence the writer gets from a quick/standard gather:
/// 1. one de-duplicated list of the search results shown during research — only the on-topic ones when
///    there are any, at most `max_hits`, without results whose page was read in full;
/// 2. the other material (Wikipedia, link lists, …);
/// 3. the pages read, last, so the most detailed material sits right before the question.
///
/// Repeated result blocks and off-topic hits cost prompt tokens (CPU time) and invite wrong citations.
pub fn consolidate(run: &Run<'_>, topics: &[&[String]], evidence: Vec<Evidence>, max_hits: usize) -> Vec<Evidence> {
    static NUM: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?m)^\[(\d+)\] ").unwrap());
    let is_search = |e: &Evidence| e.label.starts_with("web_search:") || e.label.starts_with("news_search:");
    let (searches, rest): (Vec<Evidence>, Vec<Evidence>) = evidence.into_iter().partition(|e| is_search(e));
    let mut out = vec![];
    if !searches.is_empty() {
        let mut shown: Vec<usize> = vec![];
        for e in &searches {
            for c in NUM.captures_iter(&e.text) {
                if let Ok(n) = c[1].parse::<usize>()
                    && !shown.contains(&n)
                {
                    shown.push(n);
                }
            }
        }
        let reg = run.ctx.sources.lock().unwrap();
        let hits: Vec<&crate::sources::Source> = shown
            .iter()
            .filter_map(|n| reg.get(*n))
            .filter(|s| matches!(s.kind, SourceKind::Search | SourceKind::News | SourceKind::Page))
            .collect();
        let on_topic =
            |s: &&crate::sources::Source| about(&format!("{} {} {}", s.title, s.snippet, s.url), topics, 0.25);
        // Only fall back to off-topic hits when nothing at all was on topic (they may still help).
        let any_on = hits.iter().any(on_topic);
        let list: Vec<&crate::sources::Source> =
            hits.into_iter().filter(|s| !s.read).filter(|s| !any_on || on_topic(s)).collect();
        let mut text = String::new();
        for s in list.into_iter().take(max_hits) {
            let date = s.date.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
            text.push_str(&format!("[{}] {} — {}{date}\n    URL: {}\n", s.n, s.title, s.domain, s.url));
            if !s.snippet.is_empty() {
                text.push_str(&format!("    {}\n", s.snippet.chars().take(300).collect::<String>()));
            }
        }
        if !text.is_empty() {
            out.push(Evidence { label: "Search results".into(), text });
        }
    }
    let (pages, other): (Vec<Evidence>, Vec<Evidence>) =
        rest.into_iter().partition(|e| e.label.starts_with("fetch_page:") || e.label.starts_with("crawl_site:"));
    out.extend(other);
    out.extend(pages);
    out
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
        // a page about one of two subjects counts; one sharing a single generic word doesn't
        let fw = topic_terms("how do foxes and wolves live in the wild");
        assert!(about("All about foxes", &[&fw], 0.25));
        let fr = topic_terms("what can you tell me about Friday from Iron Man and who plays her in VisionQuest");
        assert_eq!(fr, vec!["friday", "iron", "man", "visionquest"]);
        assert!(!about("Friday (1995 film) - Wikipedia", &[&fr], 0.34));
        assert!(about("Who plays Friday in VisionQuest? Tony Stark's AIs explained", &[&fr], 0.25));
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
