//! Ollama client (`/api/chat`, `/api/show`, `/api/tags`).

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{ChatModel, ChatRequest, Message, ModelInfo, Tier, strip_thinking};
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
        Self { http, cfg, caps: Mutex::new(HashMap::new()) }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.ollama_host, path)
    }

    fn model_for(&self, req: &ChatRequest) -> String {
        if let Some(m) = &req.model {
            return m.clone();
        }
        match req.tier {
            Tier::Main => self.cfg.model.clone(),
            Tier::Fast => self.cfg.fast_model().to_string(),
        }
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
            "num_ctx": self.cfg.num_ctx,
        });
        if let Some(n) = req.max_tokens {
            options["num_predict"] = json!(n);
        }
        let mut body = json!({
            "model": model,
            "messages": req.messages,
            "stream": stream,
            "keep_alive": self.cfg.keep_alive,
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

#[async_trait]
impl ChatModel for Ollama {
    fn name(&self) -> String {
        self.cfg.model.clone()
    }

    async fn chat(&self, req: ChatRequest) -> anyhow::Result<Message> {
        let (model, body) = self.body(&req, false).await;
        let resp = self
            .http
            .post(self.url("/api/chat"))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("cannot reach Ollama at {}", self.cfg.ollama_host))?;
        let status = resp.status();
        let v: Value = resp.json().await.context("invalid JSON from Ollama")?;
        if !status.is_success() {
            let err = v["error"].as_str().unwrap_or("unknown error");
            if err.contains("does not support tools") {
                self.caps.lock().await.insert(model.clone(), Caps { tools: false, thinking: false });
            }
            bail!("Ollama error ({status}) for model {model}: {err}");
        }
        let mut msg: Message = serde_json::from_value(v["message"].clone()).context("no message in reply")?;
        msg.content = strip_thinking(&msg.content);
        Ok(msg)
    }

    async fn chat_stream(
        &self,
        req: ChatRequest,
        on_token: &(dyn for<'t> Fn(&'t str) + Send + Sync),
    ) -> anyhow::Result<String> {
        let (model, body) = self.body(&req, true).await;
        let resp = self
            .http
            .post(self.url("/api/chat"))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("cannot reach Ollama at {}", self.cfg.ollama_host))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let v: Value = resp.json().await.unwrap_or_default();
            bail!("Ollama error ({status}) for model {model}: {}", v["error"].as_str().unwrap_or("unknown"));
        }
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut full = String::new();
        // Suppress leaked <think> blocks while streaming.
        let mut in_think = false;
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let v: Value = serde_json::from_str(line).map_err(|e| anyhow!("bad stream line: {e}"))?;
                if let Some(e) = v["error"].as_str() {
                    bail!("Ollama stream error: {e}");
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
                }
                full.push_str(delta);
                if delta.contains("</think>") {
                    in_think = false;
                }
            }
        }
        Ok(strip_thinking(&full))
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
                } else {
                    info.error = Some(format!("model '{want}' is not pulled — run: ollama pull {want}"));
                }
            }
            Ok(r) => info.error = Some(format!("Ollama returned {}", r.status())),
            Err(e) => info.error = Some(format!("cannot reach Ollama at {}: {e}", self.cfg.ollama_host)),
        }
        info
    }
}
