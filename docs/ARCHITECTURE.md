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
| `src/queue.rs` | the job queue: N slots, first come first served, resumed jobs at the front |
| `src/control.rs` | pause/resume of a running job: checkpoints and interruptible model calls |
| `src/watch.rs` · `src/bin/cookie-watch.rs` | the watchdog service; cookie-web's heartbeat; systemd notify/watchdog |
| `src/engine.rs` | orchestrates a request; `Run` context; the writer prompt |
| `src/depth.rs` | triage: depth levels, intents, budgets (`Profile`) |
| `src/agent.rs` | fenced tool-calling loop for quick/standard |
| `src/research.rs` | deep pipeline: plan → search → read → notes → gaps |
| `src/followup.rs` | follow-ups on a saved report; reading a report's source pages |
| `src/verify.rs` | fact check: statements that connect names, checked against the source sentences |
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

## Quick & standard: the orchestrated loop

The orchestrator decides when the model is worth calling. On a CPU every model call means re-reading its whole
prompt (the slow part), so code does everything it can do as well as the model, and the model is asked only for
judgement:

```
request ─▶ triage (heuristics; model only if unsure)
        ─▶ first searches                         [code, in parallel]
        ─▶ off-topic? rephrase + ask every engine [model writes queries, code searches]
        ─▶ read the most on-topic pages           [code, in parallel; a 2nd wave replaces dead links]
        ─▶ enough? / what else?                   [model, with the pages in front of it; its tool calls run in parallel]
              └─ repeat until DONE, budget or time is used up
        ─▶ write ─▶ fact-check ─▶ cite            [model, model, code]
```

1. **Seeded tools** — code always runs the first search (plus `wikipedia` for entities, `news_search` for news),
   so every answer is grounded even if the model never calls a tool.
2. **Rescue** — the results are checked against the request's distinctive words. If they are empty or off-topic
   (search engines often misread conversational requests), the fast model writes three rephrasings (mechanical
   ones if the model is down) and those are searched on **every** engine at once. Once something on-topic is found,
   off-topic hits are removed from what the model sees.
3. **Read first** — code reads the most on-topic results right away, in parallel (standard: 2, quick: 1, or none
   when Wikipedia already identified a name). Off-topic results are never read.
4. **Model rounds** — the model sees the results and short excerpts of those pages and decides: enough (`DONE`),
   or which tools fill the gaps (up to 4 at once, run in parallel). Duplicate calls are refused; page reads are
   budgeted. **Quick requests skip this** when code already has an on-topic page or Wikipedia article — one model
   call (the writer) is enough. Compared with letting the model ask for the first pages itself, this saves a
   model round on most requests.
5. **Floor** — if reads failed and fewer pages than wanted were read, code reads the next on-topic results.
6. **Evidence for the writer** — one de-duplicated list of on-topic search results, then other material, then
   the pages (closest to the question); pages without link URLs and footnote markers; the request is restated
   at the end.
7. **Context fitting** — before each round, older tool results are shortened so the conversation stays inside
   `COOKIE_NUM_CTX` (Ollama would otherwise silently drop the start of the prompt — the instructions).
8. **Pipeline mode** (`COOKIE_AGENT_MODE=pipeline`, **the default**) skips step 4 entirely: code searches and reads,
   the fast model triages and rephrases, and the main model is called once — to write. `COOKIE_AGENT_MODE=agent`
   lets the main model run extra tool rounds when it wants more (more adaptive, slower on a CPU).
9. **No native tool calling?** — models without Ollama `tools` support use a JSON protocol
   (`{"tool": "...", "args": {...}}` with Ollama structured output). If native calling errors at runtime
   (e.g. a malformed tool call) at any step, the loop switches to the JSON protocol automatically.
10. **Time budget** — quick/standard/deep get 2/5/15 minutes of research (`COOKIE_TIME_SCALE` scales them); when
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

## Fact check

Small models read correct sources and still cross facts between similar subjects. A real example: the sources
said "F.R.I.D.A.Y. appears as Alison Hogan, while E.D.I.T.H. takes the form of Edith Marks", and a 4B model wrote
"Friday is based on Edith Marks, played by Emily Hampshire". After the answer is written:

1. It is split into statements (sentences and list items). The ones that connect **two or more names** — where facts
   get crossed — are checked, the ones with the most names first (quick 5, standard 8, deep 12 at most).
2. For each, code collects the source sentences that mention those names (plus their neighbours, for "she" and
   "he"), matching names loosely ("F.R.I.D.A.Y." = "Friday", "Samuel Jackson" ≈ "Samuel L. Jackson").
3. The model gets one short question per statement: do these passages *support*, *contradict* or *not contain* it,
   paying attention to which name each fact belongs to. Judging one sentence against a few others is far easier for a
   small model than writing the whole answer, so it catches what the writer got wrong.
4. Contradicted or unsupported statements are removed and listed in `warnings`. If the checker would remove most of
   the answer, it only removes outright contradictions (or nothing, with a warning) — the checker can be wrong too.

Cost: a few hundred prompt tokens per statement checked. `COOKIE_FACT_CHECK=false` turns it off.

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
| the model crosses facts between names | the fact check removes the statements the sources contradict (see *Fact check*) |
| research runs long | per-depth time budget, then the answer is written from what was found |
| a job hangs | hard limit `COOKIE_JOB_TIMEOUT_SECS` |
| a job crashes | the job still ends with an `error` event; the server keeps running |
| the client disconnects | the job keeps running; streams resume with `?from=` / `Last-Event-ID` |
| the client retries a POST | `Idempotency-Key` reattaches it to the same job |
| the user changes their mind | `DELETE /api/jobs/{id}` (the GUI's *Stop*) frees the slot |
| something more urgent comes in | `POST /api/jobs/{id}/pause`: the model call is dropped (CPU free at once), the slot goes to the next job; `resume` continues at the front of the line, paused time not counted |
| too many requests pile up | the queue holds `COOKIE_MAX_QUEUE`; beyond that `503` + `Retry-After` instead of an endless wait |
| a slot leaks (crash, cancel mid-handover) | slots are released on drop and handed straight to the next live waiter; cancelled waiters are skipped |
| SIGTERM (systemd, Docker) | graceful shutdown; open streams get 10 s; the watchdog is told it's a clean stop |
| cookie-web crashes | systemd restarts it (`Restart=always`); the watchdog shows `offline` with the reason meanwhile |
| cookie-web hangs | systemd's watchdog (`WatchdogSec=60`) restarts it — it's pinged only while the HTTP server answers; cookie-watch shows `unresponsive` |
| cookie-web is down and someone opens the site | nginx sends them to the watchdog's status page (API calls get a JSON 503), and back once it's up |

Everything that went wrong along the way is listed in `result.warnings` (and streamed as `warning` events), so a
supervisor can decide whether to trust, retry or escalate.

## Safety

- **Access** — with `COOKIE_API_KEY` and/or `COOKIE_PASSWORD` set, the website asks for a password (signed,
  HttpOnly session cookie, 30 days; wrong passwords rate-limited per client) and the API needs the key, the
  password or a session. See `src/auth.rs` and [API.md → Authentication](API.md#authentication).
- **SSRF guard** — `fetch_page`/`crawl_site` refuse non-http(s) URLs and anything resolving to private,
  loopback or link-local addresses (also checked on redirects). Override with `COOKIE_ALLOW_PRIVATE_URLS=true`.
- **Limits** — page bodies capped (`COOKIE_MAX_PAGE_BYTES`), fetch timeouts, per-depth budgets derived from the
  context window (`Profile::fit_to_context`), evidence shrunk to fit.
- **Caching** — pages (30 min) and non-empty searches (15 min) are cached in memory.
- **Concurrency** — `COOKIE_MAX_CONCURRENT` jobs at a time; the rest queue first come first served (with their
  place in line), up to `COOKIE_MAX_QUEUE`.
- **Network exposure** — in the standard deployment only nginx is reachable (ufw: SSH, 80, 443); cookie-web,
  the watchdog and Ollama listen on 127.0.0.1. Forwarded client addresses are trusted only from 127.0.0.1, and
  the watchdog accepts heartbeats only from the machine itself.

## Extending

- **New tool** — implement `tools::Tool` (`name`, `description`, JSON-schema `parameters`, `call`) and register it
  in `Registry::standard`. Register URLs via `ctx.add_source(..)` so they become citable.
- **New search backend** — implement `tools::search::Backend` and add it in `SearchEngine::from_config`.
- **Another LLM runtime** — implement `llm::ChatModel` (e.g. llama.cpp server, vLLM, an OpenAI-compatible API).
