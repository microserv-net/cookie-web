# How cookie-web works

cookie-web pairs a **small** local model with **strong, deterministic tooling**. Small models are good at
reading, summarising and choosing the next step; they are bad at long unsupervised loops. So the code
does the heavy lifting (searching, ranking, extracting, budgeting, citing) and the model makes the
judgement calls.

```
             ┌──────────── request ────────────┐
             ▼                                 │
        ┌─────────┐   quick / standard   ┌────────────┐
        │ triage  │ ───────────────────▶ │ agent loop │──┐
        └─────────┘                      └────────────┘  │   evidence     ┌────────┐   ┌──────────┐
             │            deep           ┌────────────┐  ├──────────────▶ │ writer │──▶│ citation │──▶ Markdown
             └─────────────────────────▶ │  pipeline  │──┘                └────────┘   │  linker  │    + sources
                                         └────────────┘                                └──────────┘
                     tools: web_search · news_search · fetch_page · extract_links · crawl_site · wikipedia
```

## Source layout

| file | role |
|------|------|
| `src/main.rs` | CLI: `serve`, `ask` (also `--follow`), `source`, `doctor`, `tool`, `tools` |
| `src/server.rs` | axum HTTP API, jobs, SSE, auth, embedded GUI |
| `src/engine.rs` | orchestrates a request; `Run` context; the writer prompt |
| `src/depth.rs` | triage: depth levels, intents, budgets (`Profile`) |
| `src/agent.rs` | fenced tool-calling loop for quick/standard |
| `src/research.rs` | deep pipeline: plan → search → read → notes → gaps |
| `src/followup.rs` | follow-ups on a saved report; reading a report's source pages |
| `src/report.rs` | citation linking, Sources section, report storage |
| `src/sources.rs` | source registry (stable `[n]` numbers, URL de-dup) |
| `src/llm/` | `ChatModel` trait + Ollama client (native tools, JSON mode, streaming) |
| `src/tools/` | the tool belt; `net.rs` (HTTP, SSRF guard, caches), `extract.rs` (readability, PDF, focus) |
| `static/` | GUI (HTML/CSS/JS, embedded into the binary at compile time; `vendor/` holds marked and DOMPurify so the GUI needs no CDN) |

## Triage

"How severe is this request?" is answered in three layers:

1. **Explicit** — `depth` in the request, or a `!quick` / `!standard` / `!deep` prefix.
2. **Heuristics** (instant) — bare words/phrases (≤3 words, not a question) → `quick`;
   report/analysis vocabulary ("detailed", "report", "deep dive", "pros and cons", …) or very long prompts → `deep`.
   Also detects the *intent*: entity, fact, news, how-to, compare, research, links.
3. **Model** — when heuristics aren't confident, the fast model classifies with a JSON schema
   (`depth`, `intent`, `search_query`, `reason`). Any failure falls back to the heuristic.

The intent shapes the output: how-tos get numbered steps and code blocks, comparisons get a table,
news leads with dated developments, link requests lead with a curated link list.

## Quick & standard: the fenced agent loop

1. **Seeded tools** — code always runs the first search (plus `wikipedia` for entities, `news_search` for news),
   so every answer is grounded even if the model never calls a tool.
2. **Rescue** — the results are checked against the request's distinctive words. If they are empty or off-topic
   (search engines often misread conversational requests), the fast model writes three rephrasings (mechanical
   ones if the model is down) and those are searched too. Once something on-topic is found, off-topic hits are
   removed from what the model sees.
3. **Model rounds** — the model sees the results and the tool specs and may call tools (up to 4 in parallel per
   round). Duplicate calls are refused; page reads are budgeted (`fetch_page` reports when the budget is gone).
   It says `DONE` when it has enough.
4. **Deterministic floor** — if the model read too little (standard: <2 pages; quick: 1 unless Wikipedia already
   identified the subject), code reads the most on-topic readable results itself, moving past dead links.
5. **Context fitting** — before each round, older tool results are shortened so the conversation stays inside
   `COOKIE_NUM_CTX` (Ollama would otherwise silently drop the start of the prompt — the instructions).
6. **Pipeline mode** (`COOKIE_AGENT_MODE=pipeline`) skips step 3 entirely: code searches and reads the top
   results, the model only triages and writes. For slow CPUs or very small models.
7. **No native tool calling?** — models without Ollama `tools` support use a JSON protocol
   (`{"tool": "...", "args": {...}}` with Ollama structured output). If native calling errors at runtime
   (e.g. a malformed tool call) at any step, the loop switches to the JSON protocol automatically.
8. **Time budget** — quick/standard/deep get 2/5/15 minutes of research (`COOKIE_TIME_SCALE` scales them); when
   time is up the loop stops and the answer is written from what was found.

## Deep: the research pipeline

1. **Plan** — 3–6 sub-questions and 4–6 diverse queries (JSON).
2. **Search** — all queries in parallel; results merged with reciprocal-rank fusion. Few on-topic results →
   rephrase and search again.
3. **Choose** — top URLs with domain diversity (≤2 per domain), skipping JS/login-walled sites.
4. **Read** — up to 4 pages concurrently, each excerpted around the question (`focus`); failed reads are topped
   up from the next-best results.
5. **Notes** — the fast model distils each page into ≤8 factual bullets (or `NONE`); when the time budget is
   spent, raw focused excerpts are used instead.
6. **Gaps** — the model reviews coverage and proposes ≤3 follow-up queries → one more search/read/notes round.
7. **Write** — the report is written from the notes, plus snippets of unread results for breadth.

## Tools

| tool | what it does |
|------|--------------|
| `web_search` | multi-backend web search; numbered results with snippets |
| `news_search` | same, news vertical / recency-biased |
| `fetch_page` | URL → clean Markdown (readability for HTML, text extraction for PDFs), optionally only the passages relevant to `focus` (BM25-style block scoring) |
| `extract_links` | all links on a page, filterable (downloads, docs, sub-pages) |
| `crawl_site` | same-site BFS crawl (robots.txt respected, links ranked by relevance to `focus`) |
| `wikipedia` | best article summary + other candidates (disambiguation) |

### Search backends

Configured backends are tried in order (`COOKIE_SEARCH_MODE=fallback`) or all queried in parallel and fused
(`COOKIE_SEARCH_MODE=fusion`):

| backend | needs | notes |
|---------|-------|-------|
| SearXNG | `SEARXNG_URL` | **Recommended.** Self-hosted meta-search (Google/Bing/DDG/… at once). `docker compose` ships one. |
| Brave Search API | `BRAVE_API_KEY` | high quality, free tier |
| Tavily | `TAVILY_API_KEY` | LLM-oriented search API |
| Ollama web search | `OLLAMA_API_KEY` | ollama.com hosted search |
| DuckDuckGo | — | keyless HTML scraping (html + lite endpoints) |
| Bing | — | keyless HTML scraping |
| Mojeek | — | keyless, independent index |

Keyless scraping works from home connections but is often rate-limited from datacenter IPs — use SearXNG
or an API key on servers.

## Follow-ups

A follow-up (`report` set on a request) skips triage-by-model and searching-first:

```
parent report ──▶ its sources (same numbers) ──▶ full text: stored copy │ read now & store
                                               ──▶ excerpts around the question + the earlier answer
                    covered? ── no (web: auto/always) ──▶ standalone query ──▶ agent loop (new sources numbered after the old)
                                               ──▶ writer ──▶ citations kept as-is (no renumbering) ──▶ saved, `follows: <parent>`
```

"Covered" is judged in code: at least one excerpt contains half of the question's distinctive words. Questions
with no distinctive words ("tell me more", "explain the second point") are about the report itself.

Reports store the pages they read in `<id>.pages.json` (main text + images, written atomically); pages read later
for `/sources/{n}` are added to it, so content requests and follow-ups survive restarts and dead links.

## Citations

Every URL a tool touches is registered in a per-request `SourceRegistry` (de-duplicated on a normalised URL,
tracking parameters stripped) and gets a stable number shown to the model as `[n]`. The writer cites with `[n]`;
afterwards code:

- rewrites citation styles small models drift into (`[Source 3]`, `【3】`, `[^3]`, `[2-4]`) as `[n]`,
- removes any Sources/References list the model appended, and collapses passages the model repeated,
- unlinks Markdown links to URLs the research never saw (made-up links),
- turns `[n]` / `[1, 2]` / `[1][3]` into clickable `[[n]](url)` links (ignoring code blocks), dropping numbers
  that don't exist **or that the writer was never shown**,
- renumbers sources 1, 2, 3… in citation order and appends `## Sources`: cited sources first, then on-topic
  *Further reading*.

## Reliability

The goal: a request either gets a useful, honestly-labelled answer or a clear error — never a hang, never a
silent failure. Each layer has its own fallback:

| what can fail | what happens |
|---------------|--------------|
| a search engine (rate limit, captcha, outage, slow) | 15 s limit per call; after 2 failures in a row the backend rests 30 s, doubling up to 10 min, while the others take over (`/api/health` → `search_health`) |
| every search engine at once | the whole search is retried once after 1.5 s |
| one engine returns only a few results | fallback mode asks the next engine and merges until there are enough |
| a query matches nothing | quotes/operators are dropped and it is retried; a time filter is dropped if that was the cause |
| results are empty or off-topic | the request is rephrased and searched again (see *Rescue* above) |
| a page times out, 429/5xx | retried once after a short pause (honours `Retry-After`) |
| a page answers 401/403 | retried once with plain browser headers |
| a page is a bot wall, consent screen or empty JS shell | treated as a failed read, not as content (then the archive is tried) |
| a Reddit thread (JS-only) | read from old.reddit.com, which serves plain HTML |
| a page is dead or blocked | the newest Wayback Machine copy is read instead (`COOKIE_ARCHIVE_FALLBACK`) |
| a page read fails | the page budget is refunded, so the next result gets read |
| a page in a legacy encoding | decoded from the HTTP or `<meta>` charset |
| Wikipedia rate-limits | retried once after a pause |
| a tool hangs or panics | each tool call has a time limit and panics are caught; the model sees an error and moves on |
| Ollama restarting, loading or busy (connect errors, 5xx, 429) | retried after 1, 3 and 6 s |
| a malformed native tool call | no retry; the agent switches to the JSON protocol |
| an unknown model | a clear error with the `ollama pull` command |
| the answer stream breaks off | the answer is rewritten (clients get a `reset` event to drop the half answer) |
| the writer fails twice | a cited digest of the sources is returned, `degraded: true` |
| the answer hits the token limit | a dangling half-sentence is dropped and open code fences are closed |
| research runs long | per-depth time budget, then the answer is written from what was found |
| a job hangs | hard limit `COOKIE_JOB_TIMEOUT_SECS` |
| a job crashes | the job still ends with an `error` event; the server keeps running |
| the client disconnects | the job keeps running; streams resume with `?from=` / `Last-Event-ID` |
| the client retries a POST | `Idempotency-Key` reattaches it to the same job |
| the user changes their mind | `DELETE /api/jobs/{id}` (the GUI's *Stop*) frees the slot |
| SIGTERM (systemd, Docker) | graceful shutdown; open streams get 10 s |

Everything that went wrong along the way is listed in `result.warnings` (and streamed as `warning` events), so a
supervisor can decide whether to trust, retry or escalate.

## Safety

- **SSRF guard** — `fetch_page`/`crawl_site` refuse non-http(s) URLs and anything resolving to private,
  loopback or link-local addresses (also checked on redirects). Override with `COOKIE_ALLOW_PRIVATE_URLS=true`.
- **Limits** — page bodies capped (`COOKIE_MAX_PAGE_BYTES`), fetch timeouts, per-depth budgets derived from the
  context window (`Profile::fit_to_context`), evidence shrunk to fit.
- **Caching** — pages (30 min) and non-empty searches (15 min) are cached in memory.
- **Concurrency** — `COOKIE_MAX_CONCURRENT` jobs at a time; the rest queue (and say so in a `status` event).

## Extending

- **New tool** — implement `tools::Tool` (`name`, `description`, JSON-schema `parameters`, `call`) and register it
  in `Registry::standard`. Register URLs via `ctx.add_source(..)` so they become citable.
- **New search backend** — implement `tools::search::Backend` and add it in `SearchEngine::from_config`.
- **Another LLM runtime** — implement `llm::ChatModel` (e.g. llama.cpp server, vLLM, an OpenAI-compatible API).
