mod common;

use cookie_web::{
    depth::Depth,
    engine::ResearchRequest,
    events::{Event, Events},
};

async fn run(
    native: bool,
    query: &str,
    depth: Option<&str>,
) -> (cookie_web::engine::ResearchResult, Vec<Event>, Vec<String>) {
    let (engine, llm) = common::engine(native, "engine");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let req = ResearchRequest {
        query: query.into(),
        depth: depth.map(String::from),
        model: None,
        save: true,
        report: None,
        sources: None,
        web: None,
        max_words: None,
    };
    let result = engine.run(req, Events::new(tx)).await.expect("run ok");
    let mut events = vec![];
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    let log = llm.log.lock().unwrap().clone();
    (result, events, log)
}

#[tokio::test]
async fn quick_word_seeds_search_and_wikipedia() {
    let (r, events, log) = run(true, "VisionQuest", None).await;
    assert_eq!(r.depth, Depth::Quick);
    assert_eq!(r.triage.decided_by, "heuristic");
    assert!(!log.contains(&"triage".to_string()), "confident heuristic should skip the model");
    let tools: Vec<String> = events
        .iter()
        .filter_map(|e| if let Event::ToolCall { tool, .. } = e { Some(tool.clone()) } else { None })
        .collect();
    assert!(tools.contains(&"web_search".into()) && tools.contains(&"wikipedia".into()), "{tools:?}");
    // citations become links, bogus ones are dropped, model's own Sources list replaced
    assert!(r.markdown.contains("[[1]](https://"), "{}", r.markdown);
    assert!(!r.markdown.contains("[99]"));
    assert_eq!(r.markdown.matches("## Sources").count(), 1);
    assert!(!r.markdown.contains("whatever"));
    // tokens streamed, report saved with front-matter
    assert!(events.iter().any(|e| matches!(e, Event::Token { .. })));
    let path = r.report_path.clone().expect("saved");
    let file = std::fs::read_to_string(path).unwrap();
    assert!(file.starts_with("---\nid: "));
    assert!(file.contains("depth: quick"));
}

#[tokio::test]
async fn standard_uses_native_tool_calls() {
    let (r, _events, log) = run(true, "how does the fake thing work in practice today", Some("standard")).await;
    assert_eq!(r.depth, Depth::Standard);
    assert!(log.contains(&"agent-native".to_string()));
    assert!(r.sources.iter().any(|s| s.url == "https://example.com/a" && s.read), "model-requested page was read");
    assert!(r.stats.pages_read >= 2, "deterministic floor reads at least 2 pages: {:?}", r.stats);
    assert!(r.sources.iter().any(|s| s.cited), "{}\n{:?}", r.markdown, r.warnings);
}

#[tokio::test]
async fn json_fallback_without_native_tools() {
    let (r, _events, log) = run(false, "how does the fake thing work in practice today", Some("standard")).await;
    assert!(log.contains(&"agent-json".to_string()));
    assert!(r.sources.iter().any(|s| s.url == "https://example.com/b" && s.read));
}

#[tokio::test]
async fn deep_runs_the_research_pipeline() {
    let (r, events, log) = run(true, "write a detailed report on fake things", None).await;
    assert_eq!(r.depth, Depth::Deep);
    for step in ["plan", "notes", "gaps", "write"] {
        assert!(log.contains(&step.to_string()), "missing {step}: {log:?}");
    }
    let searches = events.iter().filter(|e| matches!(e, Event::ToolCall { tool, .. } if tool == "web_search")).count();
    assert!(searches >= 4, "planned + follow-up queries, got {searches}");
    assert!(r.stats.pages_read >= 3);
}

#[tokio::test]
async fn bang_prefix_overrides_depth() {
    let (r, _, _) = run(true, "!deep VisionQuest", None).await;
    assert_eq!(r.depth, Depth::Deep);
    assert_eq!(r.query, "VisionQuest");
}

#[tokio::test]
async fn rejects_bad_input() {
    let (engine, _) = common::engine(true, "bad");
    assert!(engine.run(ResearchRequest::new("   "), Events::none()).await.is_err());
    let req = ResearchRequest {
        query: "x".into(),
        depth: Some("ultra".into()),
        model: None,
        save: false,
        report: None,
        sources: None,
        web: None,
        max_words: None,
    };
    assert!(engine.run(req, Events::none()).await.is_err());
}

#[tokio::test]
async fn pipeline_mode_skips_model_tool_rounds() {
    let mut cfg = cookie_web::config::Config::for_tests(common::tmp_dir("pipeline"));
    cfg.agent_mode = cookie_web::config::AgentMode::Pipeline;
    let (engine, llm) = common::engine_with(true, cfg);
    let req = ResearchRequest {
        query: "how does the fake thing work in practice today".into(),
        depth: Some("standard".into()),
        model: None,
        save: false,
        report: None,
        sources: None,
        web: None,
        max_words: None,
    };
    let r = engine.run(req, Events::none()).await.unwrap();
    let log = llm.log.lock().unwrap().clone();
    assert!(!log.iter().any(|l| l.starts_with("agent")), "{log:?}");
    assert!(r.stats.pages_read >= 2, "code still reads the top results");
    assert!(r.markdown.contains("## Sources"));
}

#[tokio::test]
async fn quick_needs_no_model_round_when_code_found_enough() {
    let (r, _events, log) = run(true, "what is the fake thing exactly", Some("quick")).await;
    assert!(!log.iter().any(|l| l.starts_with("agent")), "an on-topic page was read, no planning call: {log:?}");
    assert_eq!(r.stats.pages_read, 1);
    assert_eq!(log.iter().filter(|l| *l == "write").count(), 1);
}

#[tokio::test]
async fn pages_are_read_in_parallel_before_the_model_decides() {
    let (_r, events, _log) = run(true, "how does the fake thing work in practice today", Some("standard")).await;
    let pos = |pred: &dyn Fn(&Event) -> bool| events.iter().position(pred).unwrap();
    let first_read = pos(&|e| matches!(e, Event::ToolCall { tool, .. } if tool == "fetch_page"));
    let deciding = pos(&|e| matches!(e, Event::Status { message } if message.starts_with("Deciding")));
    assert!(first_read < deciding, "code reads the top pages first");
    let reads_before =
        events[..deciding].iter().filter(|e| matches!(e, Event::ToolCall { tool, .. } if tool == "fetch_page")).count();
    assert_eq!(reads_before, 2, "the top two pages, read together");
}

#[tokio::test]
async fn multi_part_requests_search_and_read_every_part() {
    let mut cfg = cookie_web::config::Config::for_tests(common::tmp_dir("multipart"));
    cfg.agent_mode = cookie_web::config::AgentMode::Pipeline;
    let (engine, llm) = common::engine_with(true, cfg);
    *llm.triage_reply.lock().unwrap() = Some(
        r#"{"depth":"standard","intent":"fact","search_query":"fake thing","extra_queries":["gizmo maker"],"reason":"two parts"}"#
            .into(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let req = ResearchRequest { save: false, ..ResearchRequest::new("what is the fake thing and who made the gizmo") };
    let r = engine.run(req, Events::new(tx)).await.unwrap();
    let mut queries = vec![];
    while let Ok(e) = rx.try_recv() {
        if let Event::ToolCall { tool, args, .. } = e
            && tool == "web_search"
        {
            queries.push(args["query"].as_str().unwrap_or("").to_string());
        }
    }
    assert!(queries.contains(&"fake thing".into()) && queries.contains(&"gizmo maker".into()), "{queries:?}");
    assert_eq!(r.stats.pages_read, 3, "one more page for the second part");
    assert!(r.sources.iter().any(|s| s.read && s.url.contains("gizmo-maker")), "the second part got a page");
    let log = llm.log.lock().unwrap().clone();
    assert!(!log.iter().any(|l| l.starts_with("agent")), "pipeline: no planning rounds: {log:?}");
    assert_eq!(log.iter().filter(|l| *l == "write").count(), 1, "one main-model call: {log:?}");
}

#[tokio::test]
async fn max_words_limits_the_answer() {
    let (engine, llm) = common::engine(true, "maxwords");
    let req = ResearchRequest { save: false, max_words: Some(120), ..ResearchRequest::new("VisionQuest") };
    engine.run(req, Events::none()).await.unwrap();
    assert!(llm.write_system.lock().unwrap().contains("at most 120 words"));
    assert!(llm.write_max_tokens.lock().unwrap().unwrap() <= 252);
}
