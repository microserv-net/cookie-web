//! # cookie-web
//!
//! A lightweight web-research agent: a small local LLM (via Ollama) armed with
//! web tools, served over HTTP with a built-in GUI.
//!
//! ```text
//! request ─▶ triage (quick | standard | deep) ─▶ gather ─▶ write ─▶ cite ─▶ Markdown
//!                                         │
//!                 agent loop (quick/standard) or deep pipeline
//!                 over tools: web_search · news_search · fetch_page ·
//!                             extract_links · crawl_site · wikipedia
//! ```
//!
//! Library use:
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use std::sync::Arc;
//! use cookie_web::{config::Config, engine::{Engine, ResearchRequest}, events::Events, llm::ollama::Ollama, tools::Registry};
//! let cfg = Arc::new(Config::from_env());
//! let engine = Engine::new(cfg.clone(), Arc::new(Ollama::new(cfg.clone())), Arc::new(Registry::standard(cfg)?));
//! let result = engine.run(ResearchRequest::new("VisionQuest"), Events::none()).await?;
//! println!("{}", result.markdown);
//! # Ok(()) }
//! ```

pub mod agent;
pub mod auth;
pub mod config;
pub mod control;
pub mod depth;
pub mod engine;
pub mod events;
pub mod followup;
pub mod llm;
pub mod queue;
pub mod report;
pub mod research;
pub mod server;
pub mod sources;
pub mod tools;
pub mod verify;
pub mod watch;

use std::sync::Arc;

/// Build the production engine (Ollama + standard web tools) from a config.
pub fn build_engine(cfg: Arc<config::Config>) -> anyhow::Result<engine::Engine> {
    let llm = Arc::new(llm::ollama::Ollama::new(cfg.clone()));
    let tools = Arc::new(tools::Registry::standard(cfg.clone())?);
    Ok(engine::Engine::new(cfg, llm, tools))
}
