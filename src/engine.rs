//! The research engine: triage → gather (agent loop or deep pipeline) → write → cite.

use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    agent,
    config::Config,
    depth::{self, Depth, Intent, Profile, Triage},
    events::{Event, Events},
    llm::{ChatModel, ChatRequest, Completion, Message, Tier},
    report::{self, ReportStore},
    research,
    sources::Source,
    tools::{Registry, ToolCtx, ToolOutput, extract::truncate_chars},
};

/// What a caller asks for.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResearchRequest {
    /// The request, in natural language. A leading `!quick`, `!standard` or `!deep` forces the depth.
    pub query: String,
    /// `auto` (default), `quick`, `standard` or `deep`.
    #[serde(default)]
    pub depth: Option<String>,
    /// Override the Ollama model for this request.
    #[serde(default)]
    pub model: Option<String>,
    /// Persist the report to the reports directory (default true).
    #[serde(default = "yes")]
    pub save: bool,
}

fn yes() -> bool {
    true
}

impl ResearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self { query: query.into(), depth: None, model: None, save: true }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub tool_calls: usize,
    pub searches: usize,
    pub pages_read: usize,
    pub llm_calls: usize,
    pub duration_ms: u64,
}

/// The finished answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResearchResult {
    pub id: String,
    pub query: String,
    pub depth: Depth,
    pub intent: Intent,
    pub triage: Triage,
    /// The final answer as Markdown, including the Sources section.
    pub markdown: String,
    pub sources: Vec<Source>,
    pub stats: Stats,
    pub model: String,
    pub created_at: DateTime<Utc>,
    /// Where the `.md` file was saved, if `save` was true.
    #[serde(default)]
    pub report_path: Option<String>,
    /// Things that went wrong along the way without stopping the job (a backend down,
    /// a retried step, links removed from the answer, …). Empty when all went to plan.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// True when the model could not write the answer and `markdown` is a plain digest of the
    /// sources found instead.
    #[serde(default)]
    pub degraded: bool,
}

/// A labelled chunk of gathered material handed to the writer.
#[derive(Clone, Debug)]
pub struct Evidence {
    pub label: String,
    pub text: String,
}

#[derive(Default)]
pub struct Counters {
    pub tool_calls: AtomicUsize,
    pub searches: AtomicUsize,
    pub llm_calls: AtomicUsize,
    next_call_id: AtomicUsize,
}

/// Per-request state shared by the gathering strategies.
pub struct Run<'a> {
    pub engine: &'a Engine,
    pub ctx: ToolCtx,
    pub events: Events,
    pub counters: Counters,
    pub model: Option<String>,
    pub today: String,
    pub query: String,
    /// Soft deadline for gathering: past it, research stops and the answer is written.
    pub deadline: Instant,
    pub warnings: Mutex<Vec<String>>,
}

impl Run<'_> {
    /// Gathering time is up: finish the current step and move on to writing.
    pub fn out_of_time(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Record a non-fatal problem (shown to the caller in `warnings` and as a `warning` event).
    pub fn warn(&self, msg: impl Into<String>) {
        let msg = msg.into();
        let mut w = self.warnings.lock().unwrap();
        if !w.contains(&msg) {
            w.push(msg.clone());
            self.events.warn(msg);
        }
    }

    /// Invoke a tool with event emission and accounting.
    pub async fn tool(&self, name: &str, args: Value) -> (bool, ToolOutput) {
        let call_id = self.counters.next_call_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.counters.tool_calls.fetch_add(1, Ordering::SeqCst);
        if matches!(name, "web_search" | "news_search") {
            self.counters.searches.fetch_add(1, Ordering::SeqCst);
        }
        self.events.emit(Event::ToolCall { call_id, tool: name.to_string(), args: args.clone() });
        let (ok, out) = self.engine.tools.call(name, args, &self.ctx).await;
        self.events.emit(Event::ToolResult { call_id, tool: name.to_string(), ok, summary: out.summary.clone() });
        (ok, out)
    }

    pub async fn chat(&self, req: ChatRequest) -> anyhow::Result<Message> {
        self.counters.llm_calls.fetch_add(1, Ordering::SeqCst);
        let req = if req.model.is_none() && req.tier == Tier::Main { req.model(self.model.clone()) } else { req };
        self.engine.llm.chat(req).await
    }
}

pub struct Engine {
    pub cfg: Arc<Config>,
    pub llm: Arc<dyn ChatModel>,
    pub tools: Arc<Registry>,
    pub store: ReportStore,
}

/// Fit evidence into a character budget, shrinking the largest items first.
pub fn fit_evidence(items: &[Evidence], budget: usize) -> String {
    let total: usize = items.iter().map(|e| e.text.len() + e.label.len() + 8).sum();
    let mut caps: Vec<usize> = items.iter().map(|e| e.text.len()).collect();
    if total > budget && !items.is_empty() {
        // water-filling: find a per-item cap so the sum fits
        let mut lo = 200usize;
        let mut hi = caps.iter().copied().max().unwrap_or(200).max(200);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let sum: usize = items.iter().map(|e| e.text.len().min(mid) + e.label.len() + 8).sum();
            if sum <= budget { lo = mid } else { hi = mid - 1 }
        }
        caps = items.iter().map(|e| e.text.len().min(lo)).collect();
    }
    let mut out = String::new();
    for (e, cap) in items.iter().zip(caps) {
        out.push_str(&format!("### {}\n{}\n\n", e.label, truncate_chars(&e.text, cap)));
    }
    out
}

impl Engine {
    pub fn new(cfg: Arc<Config>, llm: Arc<dyn ChatModel>, tools: Arc<Registry>) -> Self {
        let store = ReportStore::new(cfg.reports_dir.clone());
        Self { cfg, llm, tools, store }
    }

    /// Run one research request. Never hangs: the whole job is bounded by `COOKIE_JOB_TIMEOUT_SECS`.
    pub async fn run(&self, req: ResearchRequest, events: Events) -> anyhow::Result<ResearchResult> {
        let limit = self.cfg.job_timeout;
        match tokio::time::timeout(limit, self.run_inner(req, events)).await {
            Ok(r) => r,
            Err(_) => bail!(
                "the research took longer than {}s and was stopped (raise COOKIE_JOB_TIMEOUT_SECS on slow hardware)",
                limit.as_secs()
            ),
        }
    }

    async fn run_inner(&self, req: ResearchRequest, events: Events) -> anyhow::Result<ResearchResult> {
        let started = Instant::now();
        let created_at = Utc::now();
        let query = req.query.trim().to_string();
        if query.is_empty() {
            bail!("query is empty");
        }
        if query.chars().count() > 4000 {
            bail!("query is too long (max 4000 characters)");
        }
        let today = created_at.format("%A, %B %-d, %Y").to_string();

        // 1. Triage ------------------------------------------------------------
        let forced = req.depth.as_deref().filter(|d| !d.eq_ignore_ascii_case("auto")).and_then(Depth::parse);
        if req.depth.as_deref().is_some_and(|d| !d.eq_ignore_ascii_case("auto")) && forced.is_none() {
            bail!("invalid depth '{}': use auto, quick, standard or deep", req.depth.unwrap_or_default());
        }
        let mut triage = depth::triage(self.llm.as_ref(), &query, &today).await;
        if let Some(d) = forced {
            triage.depth = d;
            triage.reason = "set explicitly by the caller".into();
            triage.decided_by = "user".into();
        }
        let clean_query = depth::split_override(&query).1;
        events.emit(Event::Triage(triage.clone()));
        let profile: Profile = triage.depth.profile().fit_to_context(self.cfg.num_ctx);
        // Leave at least 40% of the hard job limit for writing.
        let gather = Duration::from_secs_f64(profile.gather_secs as f64 * self.cfg.time_scale)
            .min(self.cfg.job_timeout.mul_f64(0.6));

        let run = Run {
            engine: self,
            ctx: ToolCtx::new(events.clone(), profile.results_per_search, profile.page_chars, profile.max_fetches),
            events: events.clone(),
            counters: Counters::default(),
            model: req.model.clone(),
            today: today.clone(),
            query: clean_query.clone(),
            deadline: started + gather,
            warnings: Mutex::new(vec![]),
        };

        // 2. Gather -----------------------------------------------------------
        let evidence = match triage.depth {
            Depth::Quick | Depth::Standard => agent::gather(&run, &triage, &profile).await?,
            Depth::Deep => research::gather(&run, &triage, &profile).await?,
        };
        if evidence.is_empty() || run.ctx.sources.lock().unwrap().is_empty() {
            bail!(
                "no usable web results were found, even after rephrasing — the search backends may be blocked or \
                 rate-limited from this network (run `cookie-web doctor`)"
            );
        }

        // 3. Write ------------------------------------------------------------
        events.status("Writing the answer");
        let (raw, degraded) = self.write_answer(&run, &triage, &profile, &clean_query, &evidence).await;

        // 4. Clean up, cite & assemble ------------------------------------------
        let known = known_urls(&run, &evidence);
        let mut body = report::strip_model_sources(&report::normalize_citations(&raw.text));
        let (collapsed, looped) = report::collapse_repeats(&body);
        if looped {
            run.warn("the model repeated itself; duplicate passages were removed");
            body = collapsed;
        }
        if raw.truncated {
            run.warn("the answer reached the length limit and may end abruptly");
            body = report::finish_truncated(&body);
        }
        let (body, removed) = report::unlink_unknown(&body, &known);
        if removed > 0 {
            run.warn(format!("removed {removed} link(s) the research never visited (likely made up by the model)"));
        }
        let (markdown, sources) = {
            let mut reg = run.ctx.sources.lock().unwrap();
            let body = report::link_citations(&body, &mut reg);
            let extra = match triage.depth {
                Depth::Quick => 2,
                Depth::Standard => 4,
                Depth::Deep => 6,
            };
            (format!("{}{}", body.trim_end(), report::sources_section(&reg, extra)), reg.all().to_vec())
        };
        if !degraded && !sources.iter().any(|s| s.cited) {
            run.warn("the answer does not cite its sources inline; see the Sources list");
        }

        let model = req.model.clone().unwrap_or_else(|| self.llm.name());
        let id = format!(
            "{}-{}-{}",
            created_at.format("%Y%m%d-%H%M%S"),
            report::slugify(&clean_query),
            &uuid::Uuid::new_v4().simple().to_string()[..6]
        );
        let pages_read = sources.iter().filter(|s| s.read).count();
        let mut result = ResearchResult {
            id,
            query: clean_query,
            depth: triage.depth,
            intent: triage.intent,
            triage,
            markdown,
            sources,
            stats: Stats {
                tool_calls: run.counters.tool_calls.load(Ordering::SeqCst),
                searches: run.counters.searches.load(Ordering::SeqCst),
                pages_read,
                llm_calls: run.counters.llm_calls.load(Ordering::SeqCst),
                duration_ms: started.elapsed().as_millis() as u64,
            },
            model,
            created_at,
            report_path: None,
            warnings: run.warnings.lock().unwrap().clone(),
            degraded,
        };
        if req.save {
            match self.store.save(&result).await {
                Ok(p) => {
                    result.report_path = Some(p.display().to_string());
                    // keep the sidecar in sync with the path
                    let _ = self.store.save(&result).await;
                }
                Err(e) => tracing::warn!("could not save report: {e:#}"),
            }
        }
        Ok(result)
    }

    /// Write the answer, never leaving the caller empty-handed:
    /// 1. stream with the full evidence pack;
    /// 2. if that fails or comes back empty, retry once with a smaller pack;
    /// 3. if the model still can't write, return a plain digest of the sources (`degraded`).
    async fn write_answer(
        &self,
        run: &Run<'_>,
        t: &Triage,
        p: &Profile,
        query: &str,
        evidence: &[Evidence],
    ) -> (Completion, bool) {
        let attempts = [(p.evidence_chars, 0.3f32), ((p.evidence_chars * 6 / 10).max(3_000), 0.2)];
        let streamed = Arc::new(AtomicBool::new(false));
        let mut last_err = String::new();
        for (i, (budget, temperature)) in attempts.into_iter().enumerate() {
            if i > 0 {
                if streamed.swap(false, Ordering::SeqCst) {
                    run.events.emit(Event::Reset { reason: format!("retrying the answer: {last_err}") });
                }
                run.events.status("Retrying the answer with less material");
            }
            let pack = fit_evidence(evidence, budget);
            match self.write(run, t, p, query, &pack, temperature, streamed.clone()).await {
                Ok(c) if c.text.trim().chars().count() >= 20 => {
                    if i > 0 {
                        run.warn(format!(
                            "the first writing attempt failed ({last_err}); the answer was written on the second try"
                        ));
                    }
                    return (c, false);
                }
                Ok(_) => last_err = "the model returned an empty answer".into(),
                Err(e) => last_err = format!("{e:#}"),
            }
            tracing::warn!("writing attempt {} failed: {last_err}", i + 1);
        }
        if streamed.load(Ordering::SeqCst) {
            run.events.emit(Event::Reset { reason: "falling back to a digest of the sources".into() });
        }
        run.warn(format!("the model could not write an answer ({last_err}); returning a digest of the sources found"));
        let digest = {
            let reg = run.ctx.sources.lock().unwrap();
            source_digest(reg.all(), query)
        };
        run.events.emit(Event::Token { text: digest.clone() });
        (Completion::new(digest), true)
    }

    #[allow(clippy::too_many_arguments)]
    async fn write(
        &self,
        run: &Run<'_>,
        t: &Triage,
        p: &Profile,
        query: &str,
        evidence: &str,
        temperature: f32,
        streamed: Arc<AtomicBool>,
    ) -> anyhow::Result<Completion> {
        let system = format!(
            "You are cookie-web, a meticulous web research assistant. Today is {today}.\n\
             Answer the user's request using ONLY the research evidence provided below.\n\n\
             Rules:\n\
             - Write in GitHub-flavored Markdown.\n\
             - Cite sources inline with their bracketed numbers right after the claim, e.g. \"released in 2024 [3]\" or \"[2][5]\". \
               Only use numbers that appear in the evidence. Never put URLs in citations.\n\
             - In link lists you may write Markdown links, using URLs exactly as they appear in the evidence.\n\
             - Never invent facts, numbers, dates, names or URLs. If the evidence is thin, outdated or conflicting, say so briefly.\n\
             - If the evidence does not answer the request, say what you found and what is still unknown.\n\
             - Do not add a Sources/References section — it is appended automatically.\n\
             - Do not talk about \"the evidence\", \"search results\" or your tools. Just answer.\n\
             - Match the user's language.\n\n\
             Length & format: {target}\n\
             Request type: {intent}",
            today = run.today,
            target = p.target,
            intent = t.intent.guidance(),
        );
        let user =
            format!("Request: {query}\n\n# Research evidence\n\n{evidence}\n---\nWrite the answer to the request now.");
        let req = ChatRequest::new(vec![Message::system(system), Message::user(user)])
            .max_tokens(p.write_tokens)
            .temperature(temperature)
            .model(run.model.clone());
        run.counters.llm_calls.fetch_add(1, Ordering::SeqCst);
        let events = run.events.clone();
        self.llm
            .chat_stream(req, &move |tok: &str| {
                streamed.store(true, Ordering::SeqCst);
                events.emit(Event::Token { text: tok.to_string() })
            })
            .await
            .context("writing the answer failed")
    }
}

/// Every URL the research actually saw (sources and URLs inside evidence), normalised,
/// plus bare site roots — links to anything else in the answer are treated as made up.
fn known_urls(run: &Run<'_>, evidence: &[Evidence]) -> HashSet<String> {
    let mut urls: Vec<String> = run.ctx.sources.lock().unwrap().all().iter().map(|s| s.url.clone()).collect();
    for e in evidence {
        urls.extend(report::urls_in(&e.text));
        urls.extend(report::urls_in(&e.label));
    }
    let mut known = HashSet::new();
    for u in urls {
        known.insert(crate::sources::normalize_url(&u));
        if let Ok(p) = url::Url::parse(&u)
            && let Some(h) = p.host_str()
        {
            known.insert(h.trim_start_matches("www.").to_ascii_lowercase());
        }
    }
    known
}

/// The answer of last resort when the model can't write: what was found, with citations.
pub fn source_digest(sources: &[Source], query: &str) -> String {
    let mut ranked: Vec<&Source> = sources.iter().collect();
    // pages actually read and Wikipedia first, then search results in rank order
    ranked.sort_by_key(|s| match s.kind {
        _ if s.read => 0,
        crate::sources::SourceKind::Wiki => 1,
        _ => 2,
    });
    let mut out = format!(
        "> **Note:** the language model was unavailable, so this is an unedited digest of what the web research \
         found for \"{}\" rather than a written answer.\n\n",
        query.replace('"', "'")
    );
    for s in ranked.iter().take(8) {
        let title = s.title.replace(['[', ']'], "");
        let snippet = truncate_chars(s.snippet.trim(), 300).replace('\n', " ");
        if snippet.is_empty() {
            out.push_str(&format!("- **{title}** — `{}` [{}]\n", s.domain, s.n));
        } else {
            out.push_str(&format!("- **{title}** — {snippet} [{}]\n", s.n));
        }
    }
    out
}

/// Compile-time guard: the gathering futures must stay `Send` so jobs can run on the
/// multi-threaded runtime (borrowed `&str` captured in stream closures breaks this).
#[allow(dead_code)]
fn _send_checks(run: &'static Run<'static>, t: &'static Triage, p: &'static Profile) {
    fn is_send<T: Send>(_: T) {}
    is_send(agent::gather(run, t, p));
    is_send(research::gather(run, t, p));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_fits_budget() {
        let items: Vec<Evidence> =
            (0..5).map(|i| Evidence { label: format!("item {i}"), text: "x ".repeat(2000) }).collect();
        let out = fit_evidence(&items, 5000);
        assert!(out.len() < 5600, "len {}", out.len());
        assert!(out.contains("item 4"));
    }
}
