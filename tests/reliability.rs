//! Failure modes: the job should still end with a useful answer (or a clear error), never hang.
mod common;

use std::{sync::Arc, sync::atomic::Ordering, time::Duration};

use common::{FakeFetch, FakeLlm, FakeSearch, FakeWiki, PanicTool};
use cookie_web::{
    config::{AgentMode, Config},
    engine::{ResearchRequest, ResearchResult},
    events::{Event, Events},
    tools::Registry,
};

fn req(query: &str, depth: Option<&str>) -> ResearchRequest {
    ResearchRequest { depth: depth.map(String::from), save: false, ..ResearchRequest::new(query) }
}

async fn run_with(
    llm: Arc<FakeLlm>,
    tools: Registry,
    cfg: Config,
    r: ResearchRequest,
) -> (anyhow::Result<ResearchResult>, Vec<Event>, Vec<String>) {
    let engine = common::engine_custom(llm.clone(), tools, cfg);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let res = engine.run(r, Events::new(tx)).await;
    let mut events = vec![];
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    let log = llm.log.lock().unwrap().clone();
    (res, events, log)
}

fn cfg(name: &str) -> Config {
    Config::for_tests(common::tmp_dir(name))
}

const STANDARD_Q: &str = "how does the fake thing work in practice today";

#[tokio::test]
async fn writer_retries_after_a_failed_attempt() {
    let llm = Arc::new(FakeLlm::new(true));
    llm.write_failures.store(1, Ordering::SeqCst);
    let (res, events, log) = run_with(llm, common::fake_registry(), cfg("retry"), req("VisionQuest", None)).await;
    let r = res.unwrap();
    assert!(!r.degraded);
    assert_eq!(log.iter().filter(|l| *l == "write").count(), 2);
    assert!(events.iter().any(|e| matches!(e, Event::Reset { .. })), "clients told to drop the half answer");
    assert!(r.markdown.contains("VisionQuest is a test subject"));
    assert!(!r.markdown.contains("Half an ans"));
    assert!(r.warnings.iter().any(|w| w.contains("second try")), "{:?}", r.warnings);
}

#[tokio::test]
async fn model_failure_still_returns_a_cited_digest() {
    let llm = Arc::new(FakeLlm::new(true));
    llm.write_failures.store(10, Ordering::SeqCst);
    let (res, events, _) = run_with(llm, common::fake_registry(), cfg("digest"), req("VisionQuest", None)).await;
    let r = res.expect("a degraded answer, not an error");
    assert!(r.degraded);
    assert!(r.markdown.contains("digest"), "{}", r.markdown);
    assert!(r.markdown.contains("[[1]](https://"), "digest cites its sources: {}", r.markdown);
    assert!(r.markdown.contains("## Sources"));
    assert!(r.warnings.iter().any(|w| w.contains("could not write")));
    assert!(events.iter().any(|e| matches!(e, Event::Warning { .. })));
}

#[tokio::test]
async fn ollama_down_still_answers_from_the_web() {
    let llm = Arc::new(FakeLlm::new(true));
    llm.down.store(true, Ordering::SeqCst);
    let (res, _, _) = run_with(llm, common::fake_registry(), cfg("down"), req(STANDARD_Q, Some("standard"))).await;
    let r = res.unwrap();
    assert!(r.degraded);
    assert!(r.stats.pages_read >= 2, "code still read pages without the model: {:?}", r.stats);
}

#[tokio::test]
async fn broken_native_tool_calls_fall_back_to_json() {
    let llm = Arc::new(FakeLlm::new(true));
    llm.native_broken.store(true, Ordering::SeqCst);
    let (res, _, log) = run_with(llm, common::fake_registry(), cfg("json"), req(STANDARD_Q, Some("standard"))).await;
    let r = res.unwrap();
    assert!(log.contains(&"agent-native".to_string()) && log.contains(&"agent-json".to_string()), "{log:?}");
    assert!(r.sources.iter().any(|s| s.url == "https://example.com/b" && s.read));
}

#[tokio::test]
async fn empty_searches_are_rephrased() {
    let llm = Arc::new(FakeLlm::new(true));
    let (res, events, log) = run_with(llm, common::fake_registry(), cfg("rephrase"), req("zq nothing", None)).await;
    let r = res.unwrap();
    assert!(log.contains(&"rephrase".to_string()), "{log:?}");
    let queries: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCall { tool, args, .. } if tool == "web_search" => args["query"].as_str().map(String::from),
            _ => None,
        })
        .collect();
    assert!(queries.contains(&"rescued query".to_string()), "{queries:?}");
    assert!(r.sources.iter().any(|s| s.url.contains("rescued-query")));
}

#[tokio::test]
async fn dead_links_do_not_use_up_the_reading_budget() {
    let mut tools = Registry::new();
    tools.add(FakeSearch("web_search"));
    tools.add(FakeSearch("news_search"));
    tools.add(FakeFetch(Some("site1.example")));
    tools.add(FakeWiki);
    let mut c = cfg("dead");
    c.agent_mode = AgentMode::Pipeline;
    let llm = Arc::new(FakeLlm::new(true));
    let (res, events, _) = run_with(llm, tools, c, req(STANDARD_Q, Some("standard"))).await;
    let r = res.unwrap();
    assert!(
        events.iter().any(|e| matches!(e, Event::ToolResult { ok: false, tool, .. } if tool == "fetch_page")),
        "a read failed"
    );
    assert!(r.stats.pages_read >= 2, "the floor moved on to the next results: {:?}", r.stats);
}

#[tokio::test]
async fn a_panicking_tool_does_not_kill_the_job() {
    let mut tools = common::fake_registry();
    tools.add(PanicTool("wikipedia"));
    let llm = Arc::new(FakeLlm::new(true));
    let (res, events, _) = run_with(llm, tools, cfg("panic"), req("VisionQuest", None)).await;
    assert!(res.is_ok());
    assert!(events.iter().any(|e| matches!(
        e,
        Event::ToolResult { ok: false, tool, summary, .. } if tool == "wikipedia" && summary.contains("internal error")
    )));
}

#[tokio::test]
async fn made_up_links_are_unlinked_and_odd_citations_fixed() {
    let llm = Arc::new(FakeLlm::new(true));
    *llm.answer.lock().unwrap() = "It is real [Source 1]. See [the docs](https://site1.example/VisionQuest) and \
        [a fake page](https://made-up.example/x) [2-3]."
        .into();
    let (res, _, _) = run_with(llm, common::fake_registry(), cfg("links"), req("VisionQuest", None)).await;
    let r = res.unwrap();
    assert!(!r.markdown.contains("made-up.example"), "{}", r.markdown);
    assert!(r.markdown.contains("a fake page"));
    assert!(r.markdown.contains("[the docs](https://site1.example/VisionQuest)"));
    assert!(r.markdown.contains("It is real [[1]](https://"), "{}", r.markdown);
    assert!(r.markdown.contains("[[2]](") && r.markdown.contains("[[3]]("));
    assert!(r.warnings.iter().any(|w| w.contains("removed 1 link")), "{:?}", r.warnings);
}

#[tokio::test]
async fn a_hung_job_times_out_with_a_clear_error() {
    let llm = Arc::new(FakeLlm::new(true));
    *llm.write_delay.lock().unwrap() = Duration::from_secs(5);
    let mut c = cfg("timeout");
    c.job_timeout = Duration::from_millis(400);
    let started = std::time::Instant::now();
    let (res, _, _) = run_with(llm, common::fake_registry(), c, req("VisionQuest", None)).await;
    let err = res.unwrap_err();
    assert!(format!("{err}").contains("COOKIE_JOB_TIMEOUT_SECS"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn nothing_found_anywhere_is_a_clear_error() {
    let mut tools = Registry::new();
    tools.add(FakeSearch("web_search"));
    tools.add(FakeFetch(None));
    // no model to rephrase, and every mechanical rephrasing still contains "nothing"
    let llm = Arc::new(FakeLlm::new(true));
    llm.down.store(true, Ordering::SeqCst);
    let engine = common::engine_custom(llm, tools, cfg("nothing"));
    let err = engine.run(req("nothing at all here", Some("quick")), Events::none()).await.unwrap_err();
    assert!(format!("{err}").contains("even after rephrasing"), "{err}");
}

/// Search that answers conversational queries with junk (like a real engine misreading them)
/// and the rephrased query with the real thing.
struct Misreading;

#[async_trait::async_trait]
impl cookie_web::tools::Tool for Misreading {
    fn name(&self) -> &'static str {
        "web_search"
    }
    fn description(&self) -> &'static str {
        "search"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"query":{"type":"string"}}})
    }
    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &cookie_web::tools::ToolCtx,
    ) -> anyhow::Result<cookie_web::tools::ToolOutput> {
        let q = args["query"].as_str().unwrap_or("");
        let hits: Vec<(String, String)> = if q.contains("rescued") {
            vec![("Glorbix the purple wombat, explained".into(), "https://wiki.example/glorbix".into())]
        } else {
            (1..=4).map(|i| (format!("Online design tool {i}"), format!("https://design{i}.example/"))).collect()
        };
        let mut text = String::new();
        let mut data = vec![];
        for (title, url) in hits {
            let n = ctx.add_source(&title, &url, "", cookie_web::sources::SourceKind::Search, None);
            text.push_str(&format!("[{n}] {title}\n"));
            data.push(serde_json::json!({ "title": title, "url": url, "snippet": "", "engine": "fake" }));
        }
        Ok(cookie_web::tools::ToolOutput::new(text, "results").with_data(serde_json::json!({ "hits": data })))
    }
}

#[tokio::test]
async fn off_topic_results_trigger_a_rescue_and_are_set_aside() {
    let mut tools = Registry::new();
    tools.add(Misreading);
    tools.add(FakeFetch(None));
    let llm = Arc::new(FakeLlm::new(true));
    let (res, _, log) = run_with(
        llm,
        tools,
        cfg("offtopic"),
        req("what can you tell me about glorbix the purple wombat", Some("quick")),
    )
    .await;
    let r = res.unwrap();
    assert!(log.contains(&"rephrase".to_string()), "{log:?}");
    let further = r.markdown.split("## Sources").nth(1).unwrap();
    assert!(!further.contains("design1.example"), "off-topic results not listed: {further}");
    assert!(r.sources.iter().any(|s| s.url == "https://wiki.example/glorbix" && s.cited), "{:?}", r.sources);
    assert!(!r.sources.iter().any(|s| s.url.contains("design") && s.cited), "off-topic results can't be cited");
}

/// Reads every page as the same short excerpt of the real VisionQuest sources.
struct FixedFetch;

const VISIONQUEST: &str = "The episodes also explore Vision's mind palace, where F.R.I.D.A.Y. and E.D.I.T.H. appear as \
    human avatars linked to Tony Stark's past. F.R.I.D.A.Y. appears as Alison Hogan, while E.D.I.T.H. takes the form \
    of Edith Marks, a fearless Stark test pilot. Orla Brady plays F.R.I.D.A.Y. in physical form. Emily Hampshire plays \
    the E.D.I.T.H. avatar. Allison Hogan was Howard Stark's office administrator.";

#[async_trait::async_trait]
impl cookie_web::tools::Tool for FixedFetch {
    fn name(&self) -> &'static str {
        "fetch_page"
    }
    fn description(&self) -> &'static str {
        "fetch"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"url":{"type":"string"}}})
    }
    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &cookie_web::tools::ToolCtx,
    ) -> anyhow::Result<cookie_web::tools::ToolOutput> {
        if !ctx.take_fetch() {
            return Ok(cookie_web::tools::ToolOutput::new("budget exhausted", "budget exhausted"));
        }
        let url = args["url"].as_str().unwrap_or("");
        let n = ctx.add_source("VisionQuest explained", url, "", cookie_web::sources::SourceKind::Page, None);
        Ok(cookie_web::tools::ToolOutput::new(
            format!("[{n}] VisionQuest explained\nURL: {url}\n\n{VISIONQUEST}"),
            "read",
        ))
    }
}

#[tokio::test]
async fn fact_check_removes_crossed_up_statements() {
    let mut tools = Registry::new();
    tools.add(FakeSearch("web_search"));
    tools.add(FixedFetch);
    let mut c = cfg("factcheck");
    c.agent_mode = AgentMode::Pipeline;
    let llm = Arc::new(FakeLlm::new(true));
    // the kind of answer a small model wrote from these sources: F.R.I.D.A.Y. and E.D.I.T.H. blended together
    *llm.answer.lock().unwrap() =
        "Orla Brady plays F.R.I.D.A.Y. in physical form [1]. In the series, Orla Brady plays \
        Friday, who is based on the real-life person Edith Marks [2].\n\n\
        - Friday is an AI avatar based on Edith Marks, played by Emily Hampshire [1].\n\
        - Friday is modeled on Allison Hogan, Howard Stark's office administrator [2].\n\n\
        Citations: [1][2]"
            .into();
    let (res, _, log) = run_with(
        llm.clone(),
        tools,
        c,
        req("what can you tell me about Friday from Iron Man and who plays her in VisionQuest", Some("standard")),
    )
    .await;
    let r = res.unwrap();
    let body = r.markdown.split("## Sources").next().unwrap();
    assert!(log.contains(&"judge".to_string()), "{log:?}");
    assert!(body.contains("Orla Brady plays F.R.I.D.A.Y. in physical form"), "{body}");
    assert!(body.contains("Friday is modeled on Allison Hogan"), "{body}");
    assert!(!body.contains("Edith Marks"), "crossed statements removed: {body}");
    assert!(!body.contains("Citations:"), "{body}");
    assert!(
        r.warnings.iter().any(|w| w.contains("removed 2 statements the sources don't support")),
        "{:?}",
        r.warnings
    );
    // the checker was shown the sentence that settles it
    let judged = llm.judged.lock().unwrap().clone();
    let crossed =
        judged.iter().find(|j| j.contains("Emily Hampshire [") || j.contains("played by Emily Hampshire")).unwrap();
    assert!(
        crossed.contains("F.R.I.D.A.Y. appears as Alison Hogan, while E.D.I.T.H. takes the form of Edith Marks"),
        "{crossed}"
    );
}

#[tokio::test]
async fn fact_check_can_be_turned_off() {
    let mut tools = Registry::new();
    tools.add(FakeSearch("web_search"));
    tools.add(FixedFetch);
    let mut c = cfg("nofactcheck");
    c.agent_mode = AgentMode::Pipeline;
    c.fact_check = false;
    let llm = Arc::new(FakeLlm::new(true));
    *llm.answer.lock().unwrap() = "Friday is an AI avatar based on Edith Marks, played by Emily Hampshire [1].".into();
    let (res, _, log) = run_with(llm, tools, c, req("who plays Friday in VisionQuest", Some("standard"))).await;
    assert!(res.unwrap().markdown.contains("Edith Marks"));
    assert!(!log.contains(&"judge".to_string()));
}

#[tokio::test]
async fn writer_gets_one_clean_list_of_search_results() {
    // the first search misreads the request (design tools); the rescue finds the real thing
    let mut tools = Registry::new();
    tools.add(Misreading);
    tools.add(FakeFetch(None));
    let llm = Arc::new(FakeLlm::new(true));
    let (res, _, _) = run_with(
        llm.clone(),
        tools,
        cfg("consolidate"),
        req("what can you tell me about glorbix the purple wombat", Some("standard")),
    )
    .await;
    res.unwrap();
    let prompt = llm.write_prompt.lock().unwrap().clone();
    let evidence = prompt.split("# Research evidence").nth(1).unwrap();
    assert!(evidence.matches("### Search results").count() <= 1, "{evidence}");
    assert!(!evidence.contains("### web_search:"), "raw result blocks replaced: {evidence}");
    assert!(!evidence.contains("design1.example"), "off-topic hits left out: {evidence}");
    // the request is restated after the evidence
    assert!(prompt.trim_end().ends_with("from the evidence above."), "{prompt}");
    assert!(prompt.rsplit("---").next().unwrap().contains("Request: what can you tell me about glorbix"));
}
