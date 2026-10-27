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
    ResearchRequest { query: query.into(), depth: depth.map(String::from), model: None, save: false }
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
