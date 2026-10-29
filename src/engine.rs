//! The research engine: triage → gather (agent loop or deep pipeline) → write → cite.

use std::{
    collections::{HashMap, HashSet},
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
    llm::{ChatModel, ChatRequest, Completion, Message, Tier, Usage},
    report::{self, ReportStore},
    research,
    sources::Source,
    tools::{
        Registry, ToolCtx, ToolOutput,
        extract::{Page, truncate_chars},
    },
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
    /// Follow up on this earlier report (its id): the query is answered from that report's sources
    /// first, keeping its citation numbers. `@3` in the query is short for `sources: [3]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    /// With `report`: only use these sources of the report (by citation number).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<usize>>,
    /// With `report`: `auto` (default — search the web only if the report's sources don't cover the
    /// question, never when `sources` is given), `never` or `always`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<String>,
    /// Upper limit on the answer's length in words (shorter answers are much faster to write).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_words: Option<u32>,
}

fn yes() -> bool {
    true
}

impl ResearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            depth: None,
            model: None,
            save: true,
            report: None,
            sources: None,
            web: None,
            max_words: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub tool_calls: usize,
    pub searches: usize,
    pub pages_read: usize,
    pub llm_calls: usize,
    pub duration_ms: u64,
    /// Tokens the model read (prompts) — on a CPU this is usually most of the work.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Tokens the model wrote.
    #[serde(default)]
    pub output_tokens: u64,
    /// Time spent inside the model, in milliseconds.
    #[serde(default)]
    pub model_ms: u64,
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
    /// For a follow-up: the id of the report it follows up on (its sources keep their numbers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follows: Option<String>,
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
    usage: Mutex<Usage>,
}

impl Counters {
    pub fn add_usage(&self, u: Usage) {
        let mut t = self.usage.lock().unwrap();
        t.prompt_tokens += u.prompt_tokens;
        t.output_tokens += u.output_tokens;
        t.model_ms += u.model_ms;
    }
    pub fn usage(&self) -> Usage {
        *self.usage.lock().unwrap()
    }
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
    /// Full pages known to this run (normalised URL → page), e.g. a parent report's stored pages.
    pub pages: Mutex<HashMap<String, Arc<Page>>>,
    /// Extra context for the writer (a follow-up's earlier question).
    pub context: Option<String>,
    /// The request as the caller wrote it, when the model sees a rewritten version (follow-ups).
    pub title: Option<String>,
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
        let msg = self.engine.llm.chat(req).await?;
        self.counters.add_usage(msg.usage);
        Ok(msg)
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
        if req.report.is_some() {
            return crate::followup::run(self, req, events, started, created_at, today).await;
        }

        // 1. Triage ------------------------------------------------------------
        let forced = req.depth.as_deref().filter(|d| !d.eq_ignore_ascii_case("auto")).and_then(Depth::parse);
        if req.depth.as_deref().is_some_and(|d| !d.eq_ignore_ascii_case("auto")) && forced.is_none() {
            bail!("invalid depth '{}': use auto, quick, standard or deep", req.depth.unwrap_or_default());
        }
        let (mut triage, triage_usage) = depth::triage(self.llm.as_ref(), &query, &today).await;
        if let Some(d) = forced {
            triage.depth = d;
            triage.reason = "set explicitly by the caller".into();
            triage.decided_by = "user".into();
        }
        let clean_query = depth::split_override(&query).1;
        events.emit(Event::Triage(triage.clone()));
        let profile: Profile = triage.depth.profile().limit_words(req.max_words).fit_to_context(self.cfg.num_ctx);
        // Leave at least 40% of the hard job limit for writing.
        let gather = Duration::from_secs_f64(profile.gather_secs as f64 * self.cfg.time_scale)
            .min(self.cfg.job_timeout.mul_f64(0.6));

        let mut ctx = ToolCtx::new(events.clone(), profile.results_per_search, profile.page_chars, profile.max_fetches);
        ctx.keep_urls = triage.intent == depth::Intent::Links;
        let run = Run {
            engine: self,
            ctx,
            events: events.clone(),
            counters: Counters::default(),
            model: req.model.clone(),
            today: today.clone(),
            query: clean_query.clone(),
            deadline: started + gather,
            warnings: Mutex::new(vec![]),
            pages: Mutex::new(HashMap::new()),
            context: None,
            title: None,
        };
        if triage_usage != Usage::default() {
            run.counters.llm_calls.fetch_add(1, Ordering::SeqCst);
            run.counters.add_usage(triage_usage);
        }

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

        self.finish(&run, triage, &profile, clean_query, &evidence, &req, started, created_at, None).await
    }

    /// Write, clean up, cite, assemble and save. `follows` = the parent report of a follow-up, whose
    /// citation numbers are kept (a normal answer is renumbered 1, 2, 3… in citation order instead).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn finish(
        &self,
        run: &Run<'_>,
        triage: Triage,
        profile: &Profile,
        clean_query: String,
        evidence: &[Evidence],
        req: &ResearchRequest,
        started: Instant,
        created_at: DateTime<Utc>,
        follows: Option<String>,
    ) -> anyhow::Result<ResearchResult> {
        let events = &run.events;
        // 3. Write ------------------------------------------------------------
        events.status("Writing the answer");
        let (raw, degraded) = self.write_answer(run, &triage, profile, &clean_query, evidence).await;

        // 4. Clean up, cite & assemble ------------------------------------------
        let known = known_urls(run, evidence);
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
        // Fact check: statements that tie names together must match what the sources say.
        let body = if self.cfg.fact_check && !degraded {
            let checkable: Vec<Evidence> =
                evidence.iter().filter(|e| !e.label.starts_with("Earlier answer")).cloned().collect();
            let max = match triage.depth {
                Depth::Quick => 5,
                Depth::Standard => 8,
                Depth::Deep => 12,
            };
            let (checked, dropped) = crate::verify::check(run, &body, &checkable, max).await;
            if !dropped.is_empty() {
                let list: Vec<String> =
                    dropped.iter().map(|d| format!("“{}”", d.chars().take(140).collect::<String>())).collect();
                run.warn(format!(
                    "removed {} statement{} the sources don't support: {}",
                    dropped.len(),
                    if dropped.len() == 1 { "" } else { "s" },
                    list.join("; ")
                ));
            }
            checked
        } else {
            body
        };
        let (markdown, sources) = {
            let mut reg = run.ctx.sources.lock().unwrap();
            // The writer saw only these numbers; anything else it cites is invented.
            let shown: HashSet<usize> = if degraded { HashSet::new() } else { shown_numbers(evidence) };
            let body = report::link_citations(&body, &mut reg, (!degraded).then_some(&shown));
            let extra = match triage.depth {
                Depth::Quick => 2,
                Depth::Standard => 4,
                Depth::Deep => 6,
            };
            let topic = agent::topic_terms(&clean_query);
            let on_topic = |s: &Source| agent::relevant(&format!("{} {} {}", s.title, s.snippet, s.url), &topic);
            // Number sources 1, 2, 3… in the order the answer cites them, then Further reading.
            // A follow-up keeps its parent's numbers, so "[3]" means the same source in both.
            let body = if follows.is_none() {
                let mut order = report::citation_order(&body);
                order.extend(report::further_reading(&reg, extra, &on_topic));
                let map = reg.renumber(&order);
                report::remap_citations(&body, &map)
            } else {
                body
            };
            (format!("{}{}", body.trim_end(), report::sources_section(&reg, extra, &on_topic)), reg.all().to_vec())
        };
        if !degraded && !sources.iter().any(|s| s.cited) {
            run.warn("the answer does not cite its sources inline; see the Sources list");
        }

        let model = req.model.clone().unwrap_or_else(|| self.llm.name());
        let clean_query = run.title.clone().unwrap_or(clean_query);
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
                prompt_tokens: run.counters.usage().prompt_tokens,
                output_tokens: run.counters.usage().output_tokens,
                model_ms: run.counters.usage().model_ms,
            },
            model,
            created_at,
            report_path: None,
            warnings: run.warnings.lock().unwrap().clone(),
            degraded,
            follows,
        };
        let st = &result.stats;
        tracing::info!(
            "answered \"{}\" ({}): {:.0}s total, {} model calls, {} prompt + {} generated tokens, {:.0}s in the model",
            result.query.chars().take(60).collect::<String>(),
            result.depth.as_str(),
            st.duration_ms as f64 / 1000.0,
            st.llm_calls,
            st.prompt_tokens,
            st.output_tokens,
            st.model_ms as f64 / 1000.0
        );
        if req.save {
            match self.store.save(&result).await {
                Ok(p) => {
                    result.report_path = Some(p.display().to_string());
                    // keep the sidecar in sync with the path
                    let _ = self.store.save(&result).await;
                    // Keep the full text of every page read, for follow-ups and `/sources/{n}`.
                    let pages = self.pages_of(run, &result.sources);
                    if !pages.is_empty()
                        && let Err(e) = self.store.save_pages(&result.id, &pages).await
                    {
                        tracing::warn!("could not save source pages: {e:#}");
                    }
                }
                Err(e) => tracing::warn!("could not save report: {e:#}"),
            }
        }
        Ok(result)
    }

    /// The full pages behind the sources that were read (from this run, or the page cache).
    fn pages_of(&self, run: &Run<'_>, sources: &[Source]) -> HashMap<usize, Page> {
        let known = run.pages.lock().unwrap();
        sources
            .iter()
            .filter(|s| s.read)
            .filter_map(|s| {
                let page = known
                    .get(&crate::sources::normalize_url(&s.url))
                    .cloned()
                    .or_else(|| self.tools.web.as_ref().and_then(|w| w.cached(&s.url)))?;
                Some((s.n, (*page).clone()))
            })
            .collect()
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
        let attempts = [(p.evidence_chars, 0.2f32), ((p.evidence_chars * 6 / 10).max(3_000), 0.1)];
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
             - Keep different people, characters and things apart. When the evidence covers several similar ones (two AIs, \
               two actors, two products), state each fact only about the one the evidence ties it to. If unsure, leave it out.\n\
             - Say each fact once, in short paragraphs. Do not repeat the answer as a list afterwards and do not add a \
               \"Citations:\" line.\n\
             - If the evidence does not answer the request, say what you found and what is still unknown.\n\
             - Do not add a Sources/References section — it is appended automatically.\n\
             - Do not talk about \"the evidence\", \"search results\" or your tools. Just answer.\n\
             - Match the user's language.\n\n\
             Length & format: {target}{limit}\n\
             Request type: {intent}",
            today = run.today,
            target = p.target,
            limit = p.max_words.map(|n| format!(" Hard limit: at most {n} words.")).unwrap_or_default(),
            intent = t.intent.guidance(),
        );
        let context = run.context.as_deref().map(|c| format!("{c}\n")).unwrap_or_default();
        // The request goes first (to read the evidence with it in mind) and last (small models follow
        // what they read most recently).
        let user = format!(
            "Request: {query}\n{context}\n# Research evidence\n\n{evidence}\n---\nRequest: {query}\n\
             Write the answer to this request now, from the evidence above."
        );
        let req = ChatRequest::new(vec![Message::system(system), Message::user(user)])
            .max_tokens(p.write_tokens)
            .temperature(temperature)
            .model(run.model.clone());
        run.counters.llm_calls.fetch_add(1, Ordering::SeqCst);
        let events = run.events.clone();
        let c = self
            .llm
            .chat_stream(req, &move |tok: &str| {
                streamed.store(true, Ordering::SeqCst);
                events.emit(Event::Token { text: tok.to_string() })
            })
            .await
            .context("writing the answer failed")?;
        run.counters.add_usage(c.usage);
        Ok(c)
    }
}

/// Citation numbers that appear (as `[n]`) in the evidence handed to the writer.
fn shown_numbers(evidence: &[Evidence]) -> HashSet<usize> {
    static N: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\[(\d{1,4})\]").unwrap());
    evidence
        .iter()
        .flat_map(|e| N.captures_iter(&e.text).chain(N.captures_iter(&e.label)).filter_map(|c| c[1].parse().ok()))
        .collect()
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
