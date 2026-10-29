# 🍪 cookie-web

**A small local LLM with state-of-the-art web tools, served on a port.**
Give it a request; it searches, reads, crawls and returns a cited Markdown answer — sized to the question.

```
"VisionQuest"                                   → two paragraphs + the best links
"how do I set up WireGuard on Ubuntu?"          → a one-page how-to with steps
"detailed report on sodium-ion vs lithium-ion"  → a multi-section research report
```

cookie-web is a single Rust binary: an HTTP API, a built-in web GUI and a CLI. It talks to
[Ollama](https://ollama.com) for the model and is built to be one specialist in a larger system of agents
(see [`/api/manifest`](docs/API.md#integrating-with-a-supervisor)).

## Features

- **Always uses its tools** — every answer is grounded in live web results; no answering from memory.
- **Severity triage** — picks `quick`, `standard` or `deep` from the request (or you force it with `!deep …`).
- **Tool belt** — `web_search`, `news_search`, `fetch_page` (HTML → clean Markdown, PDFs), `extract_links`,
  `crawl_site` (robots-aware), `wikipedia`.
- **Search backends** — SearXNG, Brave, Tavily, Ollama web search, plus keyless DuckDuckGo, Bing & Mojeek;
  fallback or rank-fusion mode.
- **Real citations** — every claim links to a numbered source; citations and links the research never saw are removed.
- **Fact check** — statements that tie names together are checked against the source sentences; ones the sources
  contradict (a small model mixing up two people, say) are removed and reported.
- **Built not to fail** — off-topic or empty searches are rephrased, failing search engines rest on a cooldown,
  dead pages are read from the Wayback Machine, Ollama hiccups are retried, and if the model can't write at all
  you still get a cited digest of what was found. Every bump on the way is reported in `warnings`.
- **Small-model friendly** — seeded searches, budgets, query-focused excerpts, deterministic fallbacks,
  JSON tool protocol for models without native tool calling.
- **Integrates anywhere** — blocking JSON, Server-Sent Events, or async jobs; reports saved as `.md` files.
- **Sign-in and status** — set a key/password and the website asks for it; a live status view shows running and
  queued jobs (with Stop), the model, and which search engines are working.
- **Follow-ups** — ask more about an answer (`@3` = about source 3) without starting over; citation numbers stay the
  same. Every source's full text (plain text, Markdown or JSON) and images are available from the report.

## Quick start

### Ubuntu, one command

```bash
git clone https://github.com/microserv-net/cookie-web && cd cookie-web
./scripts/setup-ubuntu.sh            # Ollama + qwen3:4b-instruct, then cookie-web as a service
```

That installs and tunes Ollama, pulls the model, builds cookie-web and starts it under systemd on port 8765
with a generated API key (printed at the end). The two halves also run on their own:

| script | what it does |
|--------|--------------|
| `scripts/install-ollama.sh` | checks the machine (RAM, disk, AVX2), installs Ollama, applies the memory tuning, pulls the model(s) with retries, verifies tool calling, test-loads the model |
| `scripts/install-cookie-web.sh` | builds (or takes `--binary`), creates the `cookie` user, `/etc/cookie-web.env` and the systemd service, starts it and checks `/api/health` |

Useful options: `--model NAME`, `--fast-model NAME`, `--local` (this machine only, no key), `--port N`,
`--binary PATH` (skip compiling on a slow box), `--open-firewall`. All scripts are safe to re-run; `--help` lists everything.

### Manually

```bash
# 1. A model (any Ollama model works; tool-calling ones work best)
ollama pull qwen3:4b-instruct   # 2.5 GB, writes the answers — see docs/HARDWARE.md
ollama pull qwen3:1.7b          # 1.4 GB, the fast model for small jobs (optional; set OLLAMA_MAX_LOADED_MODELS=2)

# 2. Build & run
cargo run --release             # serves on port 8765 on all interfaces (0.0.0.0)
```

Open **http://127.0.0.1:8765** (or `http://<server-ip>:8765` from another machine) for the GUI, or:

```bash
curl -s localhost:8765/api/research -H 'content-type: application/json' \
  -d '{"query":"VisionQuest"}' | jq -r .markdown
```

From the terminal, without the server:

```bash
cargo run --release -- ask "how do I set up WireGuard on Ubuntu?"
cargo run --release -- ask --depth deep --out report.md "state of open-weight small LLMs"
cargo run --release -- doctor     # checks Ollama, the model and every search backend
cargo run --release -- ask --follow <report-id> "what does @2 say about keys?"
cargo run --release -- source <report-id> 2 --text   # full text of source [2]
```

### With Docker (Ollama + SearXNG included)

```bash
docker compose up -d
docker compose exec ollama ollama pull qwen3:4b-instruct
docker compose exec ollama ollama pull qwen3:1.7b
```

## Configuration

Environment variables (a `.env` file is read too — see [`.env.example`](.env.example)):

| variable | default | |
|----------|---------|---|
| `COOKIE_HOST` / `COOKIE_PORT` | `0.0.0.0` / `8765` | listen address (`0.0.0.0` = reachable from the network; `127.0.0.1` = this machine only). On a VPS, also set `COOKIE_API_KEY` |
| `OLLAMA_HOST` | `http://127.0.0.1:11434` | |
| `COOKIE_MODEL` | `qwen3:4b-instruct` | main model (agent + writer) |
| `COOKIE_FAST_MODEL` | `qwen3:1.7b` | small model for the small jobs (triage, query rewriting, fact-check verdicts, notes); `none` = the main model does everything. Used only if pulled and Ollama can keep both loaded |
| `COOKIE_FAST_NUM_CTX` | `4096` | the fast model's context window (its jobs are small) |
| `COOKIE_THINK` | `false` | let thinking models think while writing (slower, sometimes better) |
| `COOKIE_NUM_CTX` | `8192` | context window; all prompts are sized to fit it |
| `COOKIE_NUM_THREAD` | — | CPU threads the model may use (default: all cores; see [why the CPU sits at 100%](docs/HARDWARE.md#why-the-cpu-sits-at-100-and-when-it-shouldnt)) |
| `COOKIE_KEEP_ALIVE` | `-1` | keep the model loaded forever |
| `COOKIE_WARMUP` | `true` | load the model when the server starts |
| `COOKIE_AGENT_MODE` | `auto` | `pipeline` = no model-driven tool rounds (faster on slow CPUs / tiny models) |
| `SEARXNG_URL` | — | e.g. `http://localhost:8888` (recommended) |
| `BRAVE_API_KEY` / `TAVILY_API_KEY` / `OLLAMA_API_KEY` | — | optional search APIs |
| `COOKIE_SEARCH_MODE` | `fallback` | or `fusion` (query all backends, merge rankings) |
| `COOKIE_DISABLE_SCRAPERS` | `false` | drop the keyless scrapers (DuckDuckGo, Bing, Mojeek) |
| `COOKIE_REPORTS_DIR` | `reports` | where `.md` reports are saved |
| `COOKIE_API_KEY` | — | protects the API (`Authorization: Bearer …`) and the website (it is the password unless `COOKIE_PASSWORD` is set) |
| `COOKIE_PASSWORD` | — | a separate password for the website sign-in (also accepted by the API) |
| `COOKIE_MAX_CONCURRENT` | `1` | parallel research jobs (others queue) |
| `COOKIE_ALLOW_PRIVATE_URLS` | `false` | allow reading LAN/localhost pages |
| `COOKIE_JOB_TIMEOUT_SECS` | `1800` | hard limit per research job (it fails cleanly instead of hanging) |
| `COOKIE_TIME_SCALE` | `1.0` | scales the per-depth research time budgets (e.g. `2` on a slow CPU, `0.5` on a GPU) |
| `COOKIE_ARCHIVE_FALLBACK` | `true` | read dead/blocked pages from the Wayback Machine |
| `COOKIE_FACT_CHECK` | `true` | check statements that connect names against the sources; remove unsupported ones |

### Model & hardware

Default model: **`qwen3:4b-instruct`** (Qwen3-4B-Instruct-2507, Q4_K_M, 2.5 GB) — the smallest model that
still does reliable tool calling and cited writing. Always loaded with an 8K context it uses about
**3.5 GB** in Ollama; cookie-web itself uses ~16 MB. **Minimum box: 8 GB RAM, 4-core AVX2 x86-64 CPU,
32 GB SSD, no GPU.** Full reasoning, alternatives, speed expectations, Ollama tuning and a systemd setup:
**[docs/HARDWARE.md](docs/HARDWARE.md)**.

## API

Full reference with examples: **[docs/API.md](docs/API.md)**. In short:

| endpoint | |
|----------|---|
| `POST /api/research` | blocking → `ResearchResult` JSON (or `?format=markdown`) |
| `POST /api/research/stream` | Server-Sent Events: triage, tool calls, sources, tokens, done |
| `POST /api/jobs` → `GET /api/jobs/{id}` (+`/events?from=n`) · `DELETE /api/jobs/{id}` | async jobs, resumable streams, cancel |
| `GET /api/reports[/{id}[/markdown]]` | saved reports |
| `POST /api/reports/{id}/ask` | follow-up question on a report (`@3` = about source 3) |
| `GET /api/reports/{id}/sources/{n}[/images/{k}]` · `/content` | a source's full content (json/markdown/text) and images; all sources at once |
| `GET /api/health` · `/api/manifest` · `/api/tools` | introspection |

Send an `Idempotency-Key` header and a retried request reattaches to the same job instead of starting another.

How it works inside: **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** (see *Reliability* for every failure mode handled).

## Development

```bash
cargo test          # unit + integration tests (offline: scripted LLM and fake tools)
cargo run -- tool fetch_page '{"url":"https://example.com","focus":"pricing"}'   # try a single tool
RUST_LOG=cookie_web=debug cargo run
```
