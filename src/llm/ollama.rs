//! Ollama client (`/api/chat`, `/api/show`, `/api/tags`).

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{ChatModel, ChatRequest, Completion, Message, ModelInfo, Tier, Usage, strip_thinking};
use crate::config::Config;

#[derive(Clone, Debug, Default)]
struct Caps {
    tools: bool,
    thinking: bool,
}

pub struct Ollama {
    http: reqwest::Client,
    cfg: Arc<Config>,
    caps: Mutex<HashMap<String, Caps>>,
    /// Waits between retries of a failed call (its length is the retry count).
    backoff: Vec<Duration>,
    /// Why the fast model is not used (not pulled, or evicted when both are loaded); `None` = in use.
    fast_off: std::sync::Mutex<Option<String>>,
}

impl Ollama {
    pub fn new(cfg: Arc<Config>) -> Self {
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // generation can legitimately take minutes on CPU
            .timeout(Duration::from_secs(60 * 15));
        let h = cfg.ollama_host.as_str();
        if h.contains("://localhost") || h.contains("://127.") || h.contains("://[::1]") {
            b = b.no_proxy();
        }
        let http = b.build().expect("http client");
        let backoff = [1, 3, 6].map(Duration::from_secs).to_vec();
        Self { http, cfg, caps: Mutex::new(HashMap::new()), backoff, fast_off: std::sync::Mutex::new(None) }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.ollama_host, path)
    }

    fn model_for(&self, req: &ChatRequest) -> String {
        if let Some(m) = &req.model {
            return m.clone();
        }
        match req.tier {
            Tier::Fast if self.fast_in_use() => self.cfg.fast_model().to_string(),
            _ => self.cfg.model.clone(),
        }
    }

    fn fast_in_use(&self) -> bool {
        self.cfg.has_fast_model() && self.fast_off.lock().unwrap().is_none()
    }

    /// Stop using the fast model (the main model takes over its jobs), saying why once.
    fn disable_fast(&self, why: String) {
        let mut off = self.fast_off.lock().unwrap();
        if off.is_none() {
            tracing::warn!("not using the fast model {}: {why}; the main model does its jobs", self.cfg.fast_model());
            *off = Some(why);
        }
    }

    fn is_fast(&self, model: &str) -> bool {
        self.cfg.has_fast_model() && model == self.cfg.fast_model()
    }

    /// Context window for a model: the fast model's jobs are small, so it gets its own smaller one.
    fn ctx_for(&self, model: &str) -> u32 {
        if self.is_fast(model) { self.cfg.fast_num_ctx } else { self.cfg.num_ctx }
    }

    async fn caps(&self, model: &str) -> Caps {
        if let Some(c) = self.caps.lock().await.get(model) {
            return c.clone();
        }

        match self.http.post(self.url("/api/show")).json(&json!({ "model": model })).send().await {
            Ok(r) if r.status().is_success() => {
                let v: Value = r.json().await.unwrap_or_default();
                let list: Vec<String> = v["capabilities"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let c =
                    Caps { tools: list.iter().any(|c| c == "tools"), thinking: list.iter().any(|c| c == "thinking") };
                self.caps.lock().await.insert(model.to_string(), c.clone());
                c
            }
            // unknown: assume tools so we try native first; we fall back on error.
            _ => Caps { tools: true, thinking: false },
        }
    }

    async fn body(&self, req: &ChatRequest, stream: bool) -> (String, Value) {
        let model = self.model_for(req);
        let caps = self.caps(&model).await;
        let mut options = json!({
            "temperature": req.temperature.unwrap_or(self.cfg.temperature),
            "num_ctx": self.ctx_for(&model),
        });
        if let Some(n) = req.max_tokens {
            options["num_predict"] = json!(n);
        }
        if let Some(t) = self.cfg.num_thread {
            options["num_thread"] = json!(t);
        }
        let mut body = json!({
            "model": model,
            "messages": req.messages,
            "stream": stream,
            "keep_alive": self.cfg.keep_alive_json(),
            "options": options,
        });
        if !req.tools.is_empty() && caps.tools {
            body["tools"] = Value::Array(req.tools.clone());
        }
        if let Some(f) = &req.format {
            body["format"] = f.clone();
        }
        if caps.thinking {
            // structured / tool steps never think; writing may if configured.
            let think = self.cfg.think && req.format.is_none() && req.tools.is_empty();
            body["think"] = json!(think);
        }
        (model, body)
    }
}

/// One failed attempt: either worth retrying (Ollama restarting, loading, overloaded, a network blip)
/// or not (bad request, unknown model, a malformed tool call the caller should handle).
enum Failure {
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

fn send_failure(e: reqwest::Error, host: &str) -> Failure {
    // A timeout of the whole request means generation hung for minutes: retrying would only hang again.
    let hung = e.is_timeout() && !e.is_connect();
    let err = anyhow::Error::new(e).context(format!("cannot reach Ollama at {host}"));
    if hung { Failure::Fatal(err) } else { Failure::Retry(err) }
}

/// Turn a non-2xx Ollama reply into a [`Failure`] with a helpful message.
fn status_failure(status: reqwest::StatusCode, err: &str, model: &str) -> Failure {
    let low = err.to_lowercase();
    if status == reqwest::StatusCode::NOT_FOUND || (low.contains("model") && low.contains("not found")) {
        return Failure::Fatal(anyhow!(
            "model '{model}' is not available in Ollama ({err}) — run: ollama pull {model}"
        ));
    }
    let e = anyhow!("Ollama error ({status}) for model {model}: {err}");
    // A tool call the model formatted badly: retrying the same prompt rarely helps; the agent switches protocol.
    if low.contains("tool call") || low.contains("does not support tools") {
        return Failure::Fatal(e);
    }
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        Failure::Retry(e)
    } else {
        Failure::Fatal(e)
    }
}

/// "qwen3:4b" and "qwen3:4b:latest"-style names refer to the same model.
fn same_model(a: &str, b: &str) -> bool {
    let norm = |m: &str| if m.contains(':') { m.to_string() } else { format!("{m}:latest") };
    norm(a) == norm(b)
}

impl Ollama {
    async fn load(&self, model: &str) -> anyhow::Result<()> {
        let body = json!({
            "model": model,
            "messages": [],
            "keep_alive": self.cfg.keep_alive_json(),
            // same options as real requests, or Ollama would reload the model on the first one
            "options": self.load_options(model),
        });
        let r = self.http.post(self.url("/api/chat")).json(&body).send().await?;
        if !r.status().is_success() {
            let v: Value = r.json().await.unwrap_or_default();
            bail!("could not load {model}: {}", v["error"].as_str().unwrap_or("unknown error"));
        }
        Ok(())
    }

    /// Names of the models Ollama has in memory right now.
    async fn loaded_models(&self) -> Vec<String> {
        let Ok(r) = self.http.get(self.url("/api/ps")).timeout(Duration::from_secs(5)).send().await else {
            return vec![];
        };
        let v: Value = r.json().await.unwrap_or_default();
        v["models"].as_array().into_iter().flatten().filter_map(|m| m["name"].as_str().map(String::from)).collect()
    }

    /// Options that decide how Ollama loads the model (they must match across requests).
    fn load_options(&self, model: &str) -> Value {
        let mut o = json!({ "num_ctx": self.ctx_for(model) });
        if let Some(t) = self.cfg.num_thread {
            o["num_thread"] = json!(t);
        }
        o
    }

    /// Shorter back-off, for tests.
    pub fn with_backoff(mut self, backoff: Vec<Duration>) -> Self {
        self.backoff = backoff;
        self
    }

    async fn chat_once(&self, model: &str, body: &Value) -> Result<Message, Failure> {
        let resp = self
            .http
            .post(self.url("/api/chat"))
            .json(body)
            .send()
            .await
            .map_err(|e| send_failure(e, &self.cfg.ollama_host))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| Failure::Retry(anyhow!("reading Ollama reply failed: {e}")))?;
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) if status.is_success() => return Err(Failure::Retry(anyhow!("invalid JSON from Ollama"))),
            Err(_) => Value::Null,
        };
        if !status.is_success() {
            let err = v["error"].as_str().map(String::from).unwrap_or_else(|| text.chars().take(200).collect());
            if err.contains("does not support tools") {
                self.caps.lock().await.insert(model.to_string(), Caps { tools: false, thinking: false });
            }
            return Err(status_failure(status, &err, model));
        }
        let mut msg: Message = serde_json::from_value(v["message"].clone())
            .map_err(|_| Failure::Retry(anyhow!("no message in Ollama reply")))?;
        msg.content = strip_thinking(&msg.content);
        msg.usage = Usage::from_ollama(&v);
        Ok(msg)
    }

    /// One streaming attempt. `emitted` is set once any token reached the caller,
    /// after which the attempt can no longer be retried transparently.
    async fn stream_once(
        &self,
        model: &str,
        body: &Value,
        on_token: &(dyn for<'t> Fn(&'t str) + Send + Sync),
        emitted: &mut bool,
    ) -> Result<Completion, Failure> {
        let resp = self
            .http
            .post(self.url("/api/chat"))
            .json(body)
            .send()
            .await
            .map_err(|e| send_failure(e, &self.cfg.ollama_host))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or_default();
            let err = v["error"].as_str().map(String::from).unwrap_or_else(|| text.chars().take(200).collect());
            return Err(status_failure(status, &err, model));
        }
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut full = String::new();
        let mut finished: Option<String> = None;
        let mut usage = Usage::default();
        // Suppress leaked <think> blocks while streaming.
        let mut in_think = false;
        let fail = |emitted: bool, e: anyhow::Error| if emitted { Failure::Fatal(e) } else { Failure::Retry(e) };
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| fail(*emitted, anyhow!("Ollama stream interrupted: {e}")))?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let v: Value =
                    serde_json::from_str(line).map_err(|e| fail(*emitted, anyhow!("bad stream line: {e}")))?;
                if let Some(e) = v["error"].as_str() {
                    return Err(fail(*emitted, anyhow!("Ollama stream error: {e}")));
                }
                if v["done"].as_bool() == Some(true) {
                    finished = Some(v["done_reason"].as_str().unwrap_or("stop").to_string());
                    usage = Usage::from_ollama(&v);
                }
                let delta = v["message"]["content"].as_str().unwrap_or("");
                if delta.is_empty() {
                    continue;
                }
                if delta.contains("<think>") {
                    in_think = true;
                }
                if !in_think {
                    on_token(delta);
                    *emitted = true;
                }
                full.push_str(delta);
                if delta.contains("</think>") {
                    in_think = false;
                }
            }
        }
        match finished {
            Some(reason) => Ok(Completion { text: strip_thinking(&full), truncated: reason == "length", usage }),
            None => Err(fail(*emitted, anyhow!("the Ollama stream ended before the answer was complete"))),
        }
    }
}

#[async_trait]
impl ChatModel for Ollama {
    fn name(&self) -> String {
        self.cfg.model.clone()
    }

    async fn chat(&self, req: ChatRequest) -> anyhow::Result<Message> {
        let (mut model, mut body) = self.body(&req, false).await;
        let mut attempt = 0;
        loop {
            match self.chat_once(&model, &body).await {
                Ok(m) => return Ok(m),
                // The fast model isn't pulled: hand its jobs to the main model from now on.
                Err(Failure::Fatal(e)) if self.is_fast(&model) && format!("{e}").contains("not available") => {
                    self.disable_fast(format!("it is not pulled (run: ollama pull {})", self.cfg.fast_model()));
                    (model, body) = self.body(&req, false).await;
                }
                Err(Failure::Fatal(e)) => return Err(e),
                Err(Failure::Retry(e)) => {
                    let Some(wait) = self.backoff.get(attempt) else { return Err(e) };
                    tracing::warn!("Ollama call failed ({e:#}); retrying in {:.1}s", wait.as_secs_f64());
                    tokio::time::sleep(*wait).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn chat_stream(
        &self,
        req: ChatRequest,
        on_token: &(dyn for<'t> Fn(&'t str) + Send + Sync),
    ) -> anyhow::Result<Completion> {
        let (model, body) = self.body(&req, true).await;
        let mut attempt = 0;
        loop {
            let mut emitted = false;
            match self.stream_once(&model, &body, on_token, &mut emitted).await {
                Ok(c) => return Ok(c),
                Err(Failure::Fatal(e)) => return Err(e),
                Err(Failure::Retry(e)) => {
                    let Some(wait) = self.backoff.get(attempt) else { return Err(e) };
                    tracing::warn!("Ollama stream failed ({e:#}); retrying in {:.1}s", wait.as_secs_f64());
                    tokio::time::sleep(*wait).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        // An empty chat loads a model with *our* options, so the first real request doesn't reload it.
        self.load(&self.cfg.model.clone()).await?;
        if !self.fast_in_use() {
            return Ok(());
        }
        let fast = self.cfg.fast_model().to_string();
        if let Err(e) = self.load(&fast).await {
            self.disable_fast(format!("{e:#}"));
            return Ok(());
        }
        // Ollama unloads a model to make room for another when it may keep only one
        // (OLLAMA_MAX_LOADED_MODELS=1) or memory is short. Swapping on every call would be far slower
        // than doing everything with the main model, so check that both stayed loaded.
        let loaded = self.loaded_models().await;
        if !loaded.is_empty() && !loaded.iter().any(|m| same_model(m, &self.cfg.model)) {
            self.disable_fast(
                "Ollama can't keep both models loaded (set OLLAMA_MAX_LOADED_MODELS=2 — scripts/install-ollama.sh \
                 does — or add memory)"
                    .into(),
            );
            self.load(&self.cfg.model.clone()).await?;
        }
        Ok(())
    }

    async fn supports_tools(&self, model: Option<&str>) -> bool {
        let m = model.map(String::from).unwrap_or_else(|| self.cfg.model.clone());
        self.caps(&m).await.tools
    }

    async fn info(&self) -> ModelInfo {
        let mut info = ModelInfo { model: self.cfg.model.clone(), ..Default::default() };
        match self.http.get(self.url("/api/tags")).timeout(Duration::from_secs(5)).send().await {
            Ok(r) if r.status().is_success() => {
                info.reachable = true;
                let v: Value = r.json().await.unwrap_or_default();
                info.installed = v["models"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|m| m["name"].as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let want = &self.cfg.model;
                info.available =
                    info.installed.iter().any(|m| m == want || (!want.contains(':') && m == &format!("{want}:latest")));
                if info.available {
                    let c = self.caps(want).await;
                    info.supports_tools = c.tools;
                    info.supports_thinking = c.thinking;
                    // Actual memory of the loaded model (weights + KV cache + buffers), from /api/ps.
                    if let Ok(r) = self.http.get(self.url("/api/ps")).timeout(Duration::from_secs(5)).send().await {
                        let ps: Value = r.json().await.unwrap_or_default();
                        let full = if want.contains(':') { want.clone() } else { format!("{want}:latest") };
                        if let Some(m) =
                            ps["models"].as_array().into_iter().flatten().find(|m| m["name"] == full.as_str())
                        {
                            info.loaded = true;
                            info.memory_bytes = m["size"].as_u64();
                            info.context_length = m["context_length"].as_u64();
                        }
                    }
                } else {
                    info.error = Some(format!("model '{want}' is not pulled — run: ollama pull {want}"));
                }
            }
            Ok(r) => info.error = Some(format!("Ollama returned {}", r.status())),
            Err(e) => info.error = Some(format!("cannot reach Ollama at {}: {e}", self.cfg.ollama_host)),
        }
        if self.cfg.has_fast_model() {
            let fast = self.cfg.fast_model().to_string();
            info.fast_available = info.installed.iter().any(|m| same_model(m, &fast));
            if info.reachable && !info.fast_available {
                self.disable_fast(format!("it is not pulled (run: ollama pull {fast})"));
            } else if info.fast_available {
                // pulled since: use it again
                let mut off = self.fast_off.lock().unwrap();
                if off.as_deref().is_some_and(|w| w.starts_with("it is not pulled")) {
                    tracing::info!("fast model {fast} is now available; using it for small jobs");
                    *off = None;
                }
            }
            info.fast_active = self.fast_in_use();
            info.fast_note = self.fast_off.lock().unwrap().clone();
            info.fast_model = Some(fast);
        }
        info
    }
}
