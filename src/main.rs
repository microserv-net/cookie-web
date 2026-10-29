use std::{io::Write, sync::Arc};

use clap::{Parser, Subcommand};
use cookie_web::{
    build_engine,
    config::Config,
    engine::ResearchRequest,
    events::{Event, Events},
    tools::ToolCtx,
};

#[derive(Parser)]
#[command(name = "cookie-web", version, about = "Local web-research agent (Ollama + web tools)")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP API + GUI (default).
    Serve {
        #[arg(long, env = "COOKIE_HOST")]
        host: Option<String>,
        #[arg(long, short, env = "COOKIE_PORT")]
        port: Option<u16>,
        #[arg(long, short, env = "COOKIE_MODEL")]
        model: Option<String>,
    },
    /// Ask a question from the terminal; prints Markdown to stdout.
    Ask {
        query: Vec<String>,
        /// auto | quick | standard | deep
        #[arg(long, short, default_value = "auto")]
        depth: String,
        #[arg(long, short)]
        model: Option<String>,
        /// Also write the Markdown to this file.
        #[arg(long, short)]
        out: Option<std::path::PathBuf>,
        /// Print the full JSON result instead of Markdown.
        #[arg(long)]
        json: bool,
        /// Don't save into the reports directory.
        #[arg(long)]
        no_save: bool,
        /// Follow up on an earlier report (its id); `@3` in the question targets source [3].
        #[arg(long)]
        follow: Option<String>,
        /// With --follow: only these sources, e.g. 1,3
        #[arg(long, value_delimiter = ',')]
        sources: Option<Vec<usize>>,
        /// With --follow: auto | never | always search the web
        #[arg(long)]
        web: Option<String>,
    },
    /// Print the full content of source N of a saved report (read and stored on first use).
    Source {
        report: String,
        n: usize,
        /// Only the passages relevant to this
        #[arg(long)]
        focus: Option<String>,
        /// Plain text instead of Markdown
        #[arg(long)]
        text: bool,
    },
    /// Check Ollama, the model and the search backends.
    Doctor,
    /// Run a single tool, e.g. `cookie-web tool web_search '{"query":"rust"}'`.
    Tool { name: String, args: Option<String> },
    /// Print the tool schemas sent to the model.
    Tools,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cookie_web=info,tower_http=warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let mut cfg = Config::from_env();

    match cli.cmd.unwrap_or(Cmd::Serve { host: None, port: None, model: None }) {
        Cmd::Serve { host, port, model } => {
            if let Some(h) = host {
                cfg.host = h;
            }
            if let Some(p) = port {
                cfg.port = p;
            }
            if let Some(m) = model {
                cfg.model = m;
            }
            let engine = Arc::new(build_engine(Arc::new(cfg))?);
            let info = engine.llm.info().await;
            if let Some(e) = &info.error {
                tracing::warn!("{e}");
            } else if !info.supports_tools {
                tracing::warn!("model {} has no native tool calling; using the JSON fallback protocol", info.model);
            }
            if engine.cfg.warmup && info.available {
                let e = engine.clone();
                tokio::spawn(async move {
                    let t = std::time::Instant::now();
                    match e.llm.warmup().await {
                        Ok(()) => {
                            let mem = e
                                .llm
                                .info()
                                .await
                                .memory_bytes
                                .map(|b| format!(" — {:.2} GB in memory", b as f64 / 1e9));
                            tracing::info!(
                                "model {} loaded in {:.1}s{}",
                                e.cfg.model,
                                t.elapsed().as_secs_f64(),
                                mem.unwrap_or_default()
                            );
                        }
                        Err(err) => tracing::warn!("warm-up failed: {err:#}"),
                    }
                });
            }
            cookie_web::server::serve(engine).await
        }
        Cmd::Source { report, n, focus, text } => {
            let engine = build_engine(Arc::new(cfg))?;
            let r = engine.store.get(&report).await.ok_or_else(|| anyhow::anyhow!("no report '{report}'"))?;
            let page = cookie_web::followup::source_page(&engine, &r, n).await?;
            let md = match focus.as_deref() {
                Some(f) => cookie_web::tools::extract::focus_excerpt(&page.markdown, Some(f), 12_000),
                None => page.markdown.clone(),
            };
            let body = format!("# {}\n\nSource [{n}]: {}\n\n{md}", page.title, page.url);
            println!("{}", if text { cookie_web::report::md_to_text(&body) } else { body });
            for (i, img) in page.images.iter().enumerate() {
                eprintln!("image {}: {} {}", i + 1, img.url, img.alt);
            }
            Ok(())
        }
        Cmd::Ask { query, depth, model, out, json, no_save, follow, sources, web } => {
            let q = query.join(" ");
            if q.trim().is_empty() {
                anyhow::bail!("usage: cookie-web ask <question>");
            }
            let engine = build_engine(Arc::new(cfg))?;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let printer = tokio::spawn(async move {
                let mut err = std::io::stderr();
                while let Some(e) = rx.recv().await {
                    match e {
                        Event::Triage(t) => {
                            let _ =
                                writeln!(err, "» depth: {} · intent: {:?} — {}", t.depth.as_str(), t.intent, t.reason);
                        }
                        Event::Status { message } => {
                            let _ = writeln!(err, "» {message}");
                        }
                        Event::ToolCall { tool, args, .. } => {
                            let _ = writeln!(err, "  ↳ {tool} {args}");
                        }
                        Event::ToolResult { ok, summary, .. } => {
                            let _ = writeln!(err, "    {} {summary}", if ok { "✓" } else { "✗" });
                        }
                        Event::Warning { message } => {
                            let _ = writeln!(err, "! {message}");
                        }
                        _ => {}
                    }
                }
            });
            let req =
                ResearchRequest { query: q, depth: Some(depth), model, save: !no_save, report: follow, sources, web };
            let result = engine.run(req, Events::new(tx)).await;
            let _ = printer.await;
            let result = result?;
            if let Some(path) = out {
                std::fs::write(&path, &result.markdown)?;
                eprintln!("» wrote {}", path.display());
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", result.markdown);
            }
            if result.degraded {
                eprintln!("! degraded: the model could not write the answer; printed a digest of the sources");
            }
            eprintln!(
                "» {} sources · {} tool calls · {:.1}s{}",
                result.sources.len(),
                result.stats.tool_calls,
                result.stats.duration_ms as f64 / 1000.0,
                result.report_path.map(|p| format!(" · saved {p}")).unwrap_or_default()
            );
            Ok(())
        }
        Cmd::Doctor => doctor(cfg).await,
        Cmd::Tool { name, args } => {
            let engine = build_engine(Arc::new(cfg))?;
            let args: serde_json::Value = serde_json::from_str(args.as_deref().unwrap_or("{}"))?;
            let ctx = ToolCtx::new(Events::none(), 8, 8000, 10);
            let (ok, out) = engine.tools.call(&name, args, &ctx).await;
            println!("{}", out.text);
            eprintln!("{} {}", if ok { "✓" } else { "✗" }, out.summary);
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Tools => {
            let engine = build_engine(Arc::new(cfg))?;
            println!("{}", serde_json::to_string_pretty(&engine.tools.specs())?);
            Ok(())
        }
    }
}

async fn doctor(cfg: Config) -> anyhow::Result<()> {
    let cfg = Arc::new(cfg);
    let engine = build_engine(cfg.clone())?;
    println!("cookie-web {}", env!("CARGO_PKG_VERSION"));
    println!("ollama   {}", cfg.ollama_host);
    let info = engine.llm.info().await;
    let mark = |b: bool| if b { "✓" } else { "✗" };
    println!("  {} reachable", mark(info.reachable));
    println!("  {} model {} pulled", mark(info.available), info.model);
    println!("  {} native tool calling", mark(info.supports_tools));
    match &info.fast_model {
        Some(f) if info.fast_active => println!("  ✓ fast model {f} for small jobs (context {})", cfg.fast_num_ctx),
        Some(f) => println!("  ✗ fast model {f} not used: {}", info.fast_note.as_deref().unwrap_or("not available")),
        None => println!("  · no fast model: the main model does every job (set COOKIE_FAST_MODEL)"),
    }
    println!("  context {} tokens · keep_alive {} · agent mode {:?}", cfg.num_ctx, cfg.keep_alive, cfg.agent_mode);
    match (info.loaded, info.memory_bytes) {
        (true, Some(b)) => println!("  ✓ loaded — {:.2} GB in memory (weights + KV cache)", b as f64 / 1e9),
        _ => println!("  · not loaded right now (it loads on the first request or at `serve` start)"),
    }
    if let Some(e) = &info.error {
        println!("  ! {e}");
    }
    if !info.installed.is_empty() {
        println!("  installed: {}", info.installed.join(", "));
    }
    let engines = cookie_web::tools::search::SearchEngine::from_config(&cfg);
    println!("search   mode={:?} backends={}", cfg.search_mode, engines.names().join(", "));
    let web = cookie_web::tools::net::Web::new(cfg.clone())?;
    for b in &engines.backends {
        let q = cookie_web::tools::search::Query {
            text: "rust programming language".into(),
            max: 3,
            time_range: None,
            kind: cookie_web::tools::search::Kind::Web,
            broad: false,
        };
        match b.search(&web, &q).await {
            Ok(h) if !h.is_empty() => println!("  ✓ {:<10} {} results (e.g. {})", b.name(), h.len(), h[0].url),
            Ok(_) => println!("  ✗ {:<10} no results (possibly rate-limited)", b.name()),
            Err(e) => println!("  ✗ {:<10} {e:#}", b.name()),
        }
    }
    match web.page("https://en.wikipedia.org/wiki/Cookie").await {
        Ok(p) => println!("fetch    ✓ read \"{}\" ({} chars)", p.title, p.markdown.len()),
        Err(e) => println!("fetch    ✗ {e:#}"),
    }
    println!("reports  {}", cfg.reports_dir.display());
    Ok(())
}
