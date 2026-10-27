//! Progress events emitted while a research job runs. These are what the
//! SSE endpoints stream; see `docs/API.md` for the wire format.

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::{depth::Triage, engine::ResearchResult, sources::Source};

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The job was accepted.
    Started { job_id: String, query: String },
    /// How deep the engine decided to go, and why.
    Triage(Triage),
    /// Human-readable progress line.
    Status { message: String },
    /// A tool is being invoked.
    ToolCall { call_id: usize, tool: String, args: Value },
    /// A tool finished.
    ToolResult { call_id: usize, tool: String, ok: bool, summary: String },
    /// A new source was registered (citation number assigned).
    Source { source: Source },
    /// A chunk of the final markdown answer.
    Token { text: String },
    /// The finished result.
    Done { result: Box<ResearchResult> },
    /// Fatal error; no `done` follows.
    Error { message: String },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Started { .. } => "started",
            Event::Triage(_) => "triage",
            Event::Status { .. } => "status",
            Event::ToolCall { .. } => "tool_call",
            Event::ToolResult { .. } => "tool_result",
            Event::Source { .. } => "source",
            Event::Token { .. } => "token",
            Event::Done { .. } => "done",
            Event::Error { .. } => "error",
        }
    }
}

/// Cheap, cloneable event sink. A sink with no receiver silently drops events.
#[derive(Clone, Default)]
pub struct Events {
    tx: Option<UnboundedSender<Event>>,
}

impl Events {
    pub fn new(tx: UnboundedSender<Event>) -> Self {
        Self { tx: Some(tx) }
    }
    pub fn none() -> Self {
        Self { tx: None }
    }
    pub fn emit(&self, e: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(e);
        }
    }
    pub fn status(&self, msg: impl Into<String>) {
        self.emit(Event::Status { message: msg.into() });
    }
}
