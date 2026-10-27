//! LLM abstraction. The engine talks to a [`ChatModel`]; the production
//! implementation is [`ollama::Ollama`], tests use scripted fakes.

pub mod ollama;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For `role == "tool"`: the tool whose output this is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl Message {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self { role: role.into(), content: content.into(), tool_calls: vec![], tool_name: None }
    }
    pub fn system(c: impl Into<String>) -> Self {
        Self::new("system", c)
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self::new("user", c)
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Self::new("assistant", c)
    }
    pub fn tool(name: &str, c: impl Into<String>) -> Self {
        Self { tool_name: Some(name.into()), ..Self::new("tool", c) }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub function: FunctionCall,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

impl ToolCall {
    pub fn new(name: &str, arguments: Value) -> Self {
        Self { function: FunctionCall { name: name.into(), arguments } }
    }
}

/// Which model tier a request should run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tier {
    /// The main model (agent loop and final writing).
    #[default]
    Main,
    /// The fast model (triage, planning, note-taking). Falls back to main.
    Fast,
}

#[derive(Clone, Debug, Default)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    /// Ollama-style tool specs (`{"type":"function","function":{...}}`).
    pub tools: Vec<Value>,
    /// `"json"` or a JSON schema, for structured output.
    pub format: Option<Value>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub tier: Tier,
    /// Per-request model override (takes precedence over the tier).
    pub model: Option<String>,
}

impl ChatRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self { messages, ..Default::default() }
    }
    pub fn tier(mut self, t: Tier) -> Self {
        self.tier = t;
        self
    }
    pub fn json(mut self, schema: Value) -> Self {
        self.format = Some(schema);
        self
    }
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }
    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }
    pub fn tools(mut self, tools: Vec<Value>) -> Self {
        self.tools = tools;
        self
    }
    pub fn model(mut self, m: Option<String>) -> Self {
        self.model = m;
        self
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ModelInfo {
    pub reachable: bool,
    pub model: String,
    pub available: bool,
    pub supports_tools: bool,
    pub supports_thinking: bool,
    pub installed: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[async_trait]
pub trait ChatModel: Send + Sync {
    /// Name of the main model.
    fn name(&self) -> String;

    /// One non-streaming chat completion.
    async fn chat(&self, req: ChatRequest) -> anyhow::Result<Message>;

    /// Streaming completion; `on_token` is called with each content delta.
    /// Returns the full text.
    async fn chat_stream(
        &self,
        req: ChatRequest,
        on_token: &(dyn for<'t> Fn(&'t str) + Send + Sync),
    ) -> anyhow::Result<String>;

    /// Whether the given model (or the main one) supports native tool calling.
    async fn supports_tools(&self, model: Option<&str>) -> bool;

    /// Health / capability information.
    async fn info(&self) -> ModelInfo;
}

/// Strip `<think>…</think>` blocks that some reasoning models leak into content.
pub fn strip_thinking(s: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?s)<think>.*?(</think>|$)").unwrap());
    RE.replace_all(s, "").trim().to_string()
}

/// Best-effort JSON extraction from a model reply (handles code fences and chatter).
pub fn parse_json_loose(s: &str) -> Option<Value> {
    let s = strip_thinking(s);
    if let Ok(v) = serde_json::from_str::<Value>(&s) {
        return Some(v);
    }
    let start = s.find(['{', '['])?;
    let end = s.rfind(['}', ']'])?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&s[start..=end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_think_blocks() {
        assert_eq!(strip_thinking("<think>hmm\nok</think>\nHello"), "Hello");
        assert_eq!(strip_thinking("<think>never closed"), "");
    }

    #[test]
    fn loose_json() {
        let v = parse_json_loose("Sure!\n```json\n{\"a\": 1}\n```").unwrap();
        assert_eq!(v["a"], 1);
        assert!(parse_json_loose("nope").is_none());
    }
}
