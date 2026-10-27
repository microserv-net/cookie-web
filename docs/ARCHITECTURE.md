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
| `src/main.rs` | CLI: `serve`, `ask`, `doctor`, `tool`, `tools` |
| `src/server.rs` | axum HTTP API, jobs, SSE, auth, embedded GUI |
| `src/engine.rs` | orchestrates a request; `Run` context; the writer prompt |
| `src/depth.rs` | triage: depth levels, intents, budgets (`Profile`) |
| `src/agent.rs` | fenced tool-calling loop for quick/standard |
| `src/research.rs` | deep pipeline: plan → search → read → notes → gaps |
| `src/report.rs` | citation linking, Sources section, report storage |
| `src/sources.rs` | source registry (stable `[n]` numbers, URL de-dup) |
| `src/llm/` | `ChatModel` trait + Ollama client (native tools, JSON mode, streaming) |
| `src/tools/` | the tool belt; `net.rs` (HTTP, SSRF guard, caches), `extract.rs` (readability, PDF, focus) |
| `static/` | GUI (HTML/CSS/JS, embedded into the binary at compile time) |

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
2. **Model rounds** — the model sees the results and the tool specs and may call tools (up to 4 in parallel per
   round). Duplicate calls are refused; page reads are budgeted (`fetch_page` reports when the budget is gone).
   It says `DONE` when it has enough.
3. **Deterministic floor** — if the model read too little (standard: <2 pages; quick fact questions: 0), code reads
   the top readable results itself.
4. **Context fitting** — before each round, older tool results are shortened so the conversation stays inside
   `COOKIE_NUM_CTX` (Ollama would otherwise silently drop the start of the prompt — the instructions).
5. **Pipeline mode** (`COOKIE_AGENT_MODE=pipeline`) skips step 2 entirely: code searches and reads the top
   results, the model only triages and writes. For slow CPUs or very small models.
6. **No native tool calling?** — models without Ollama `tools` support use a JSON protocol
   (`{"tool": "...", "args": {...}}` with Ollama structured output). If native calling errors at runtime, the loop
   switches to the JSON protocol automatically.

## Deep: the research pipeline

1. **Plan** — 3–6 sub-questions and 4–6 diverse queries (JSON).
2. **Search** — all queries in parallel; results merged with reciprocal-rank fusion.
3. **Choose** — top URLs with domain diversity (≤2 per domain), skipping JS/login-walled sites.
4. **Read** — up to 4 pages concurrently, each excerpted around the question (`focus`).
5. **Notes** — the fast model distils each page into ≤8 factual bullets (or `NONE`).
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
| Mojeek | — | keyless, independent index |

Keyless scraping works from home connections but is often rate-limited from datacenter IPs — use SearXNG
or an API key on servers.

## Citations

Every URL a tool touches is registered in a per-request `SourceRegistry` (de-duplicated on a normalised URL)
and gets a stable number shown to the model as `[n]`. The writer cites with `[n]`; afterwards code:

- removes any Sources/References list the model appended,
- turns `[n]` / `[1, 2]` / `[1][3]` into clickable `[[n]](url)` links (ignoring code blocks),
- drops numbers that don't exist (hallucinated citations),
- appends a generated `## Sources` section: cited sources first, then *Further reading*.

## Safety & robustness

- **SSRF guard** — `fetch_page`/`crawl_site` refuse non-http(s) URLs and anything resolving to private,
  loopback or link-local addresses (also checked on redirects). Override with `COOKIE_ALLOW_PRIVATE_URLS=true`.
- **Limits** — page bodies capped (`COOKIE_MAX_PAGE_BYTES`), fetch timeouts, per-depth budgets derived from the
  context window (`Profile::fit_to_context`), evidence shrunk to fit.
- **Caching** — pages (30 min) and searches (15 min) are cached in memory.
- **Concurrency** — `COOKIE_MAX_CONCURRENT` jobs at a time; the rest queue.

## Extending

- **New tool** — implement `tools::Tool` (`name`, `description`, JSON-schema `parameters`, `call`) and register it
  in `Registry::standard`. Register URLs via `ctx.add_source(..)` so they become citable.
- **New search backend** — implement `tools::search::Backend` and add it in `SearchEngine::from_config`.
- **Another LLM runtime** — implement `llm::ChatModel` (e.g. llama.cpp server, vLLM, an OpenAI-compatible API).
