# cookie-web HTTP API

Base URL: behind nginx (what `scripts/setup-ubuntu.sh` sets up) it is `http(s)://<server>/`; run on its own,
`http://<server>:<COOKIE_PORT>` (port 8765). The always-on status endpoint of the watchdog is `/watch/status`
([below](#service-status-the-watchdog)).
All request/response bodies are JSON (UTF-8) unless noted. The GUI at `/` uses exactly these endpoints.

- [Authentication](#authentication)
- [The request object](#the-request-object)
- [The result object](#the-result-object)
- [Endpoints](#endpoints)
- [The queue: pause, resume, cancel](#the-queue-pause-resume-cancel)
- [Service status (the watchdog)](#service-status-the-watchdog)
- [Follow-ups and source content](#follow-ups-and-source-content)
- [Streaming events (SSE)](#streaming-events-sse)
- [Errors](#errors)
- [Reliability guarantees](#reliability-guarantees)
- [Client examples](#client-examples)
- [Integrating with a supervisor](#integrating-with-a-supervisor)

---

## Authentication

Off by default. Set `COOKIE_API_KEY` (for programs) and optionally `COOKIE_PASSWORD` (for people) to turn it on:

| | without signing in | how to get in |
|---|---|---|
| **website** (`/`, reports, …) | redirected to `/login` | the password: `COOKIE_PASSWORD`, or the API key if no password is set |
| **API** (`/api/*`) | `401 {"error": …}` | the API key or the password, as below — or the website's session cookie |
| `/login`, `/api/manifest`, page assets | open | — |
| `/api/health` | open, but only `{"status","version","auth_required"}` | full details once authorized |

```
Authorization: Bearer <key>
X-API-Key: <key>
?api_key=<key>             # query param, for EventSource clients that can't set headers
```

Signing in (`POST /login`, form field `password`, or JSON `{"password": "…"}`) sets an HttpOnly,
`SameSite=Strict` session cookie valid for 30 days. Sessions are signed with a key derived from the secrets, so
they survive restarts, and changing the password or API key signs everyone out. `GET /logout` signs out.
After 5 wrong passwords from one client in 15 minutes, sign-in is refused for the rest of that window
(`429` for JSON, `/login?error=locked` for the form). Behind the local reverse proxy the client address is taken from
`X-Real-IP` / `X-Forwarded-For` — only when the connection comes from this machine, so remote clients can't fake it —
and the cookie gets `Secure` when `X-Forwarded-Proto: https`.

> Over plain `http://` the password travels unencrypted. On a VPS, use HTTPS: `scripts/setup-nginx.sh --domain …`
> (Let's Encrypt), or reach it over a VPN / SSH tunnel.

## The request object

`POST` bodies for `/api/research`, `/api/research/stream` and `/api/jobs`:

| field   | type    | default | meaning |
|---------|---------|---------|---------|
| `query` | string  | —       | **Required.** Natural-language request (1–4000 chars). A leading `!quick`, `!standard` or `!deep` forces the depth. |
| `depth` | string  | `auto`  | `auto`, `quick`, `standard` or `deep`. Anything else → 400. |
| `model` | string  | server default | Ollama model for the agent loop and the writer (e.g. `qwen3:4b-instruct`). |
| `save`  | bool    | `true`  | Persist the report (`.md` + `.json`, plus the read pages) to `COOKIE_REPORTS_DIR`. |
| `report` | string | — | Make this a **follow-up** on that saved report (its `id`). See [Follow-ups](#follow-ups-and-source-content). |
| `sources` | [int] | — | With `report`: only use these sources of the report. `@3` in the query does the same. |
| `web` | string | `auto` | With `report`: `auto` (search the web only if the report's sources don't cover the question; never when sources are named), `never` or `always`. |
| `max_words` | int | — | Upper limit on the answer's length (20–5000). Shorter answers are faster: the writer's output budget shrinks with it. |

```json
{ "query": "VisionQuest", "depth": "auto" }
{ "query": "what does @3 say about pricing?", "report": "20261028-014512-visionquest-3fa9c1" }
```

### Depth levels

| depth | what you get | budget |
|-------|--------------|--------|
| `quick` | 1–2 paragraphs + a short list of the best links | seeded search, ≤3 agent rounds, ≤1 page read |
| `standard` | ~300–700 words with sections | ≤6 agent rounds, ≤4 page reads (≥2 guaranteed) |
| `deep` | 1200–2500-word report: TL;DR, sections, tables, caveats | planned 4–6 queries, ≤10 page reads, per-source notes, a gap-filling round |

With `auto`, cookie-web decides (see [ARCHITECTURE.md](ARCHITECTURE.md#triage)) and reports its
reasoning in `triage`.

## The result object

`ResearchResult`, returned by `/api/research`, inside the `done` event, by `/api/jobs/{id}` and `/api/reports/{id}`:

```jsonc
{
  "id": "20261028-014512-visionquest-3fa9c1",   // also the report file name
  "query": "VisionQuest",
  "depth": "quick",                              // quick | standard | deep
  "intent": "entity",                            // entity | fact | news | howto | compare | research | links
  "triage": {
    "depth": "quick",
    "intent": "entity",
    "search_query": "VisionQuest",
    "reason": "a bare word/phrase: identify it and point to the best links",
    "decided_by": "heuristic"                    // user | heuristic | model
  },
  "markdown": "VisionQuest is an upcoming Marvel Studios miniseries … [[1]](https://en.wikipedia.org/wiki/VisionQuest)\n\n---\n\n## Sources\n\n1. [VisionQuest — Wikipedia](https://en.wikipedia.org/wiki/VisionQuest) — `en.wikipedia.org`\n…",
  "sources": [
    {
      "n": 1,                                    // citation number used in the markdown
      "title": "VisionQuest — Wikipedia",
      "url": "https://en.wikipedia.org/wiki/VisionQuest",
      "domain": "en.wikipedia.org",
      "snippet": "VisionQuest is an American television miniseries …",
      "kind": "wiki",                            // search | news | page | wiki
      "read": false,                             // full page was read (not just a snippet)
      "date": null,
      "cited": true                              // the answer cites it
    }
  ],
  "stats": { "tool_calls": 2, "searches": 1, "pages_read": 0, "llm_calls": 1, "duration_ms": 8421,
             "prompt_tokens": 3120, "output_tokens": 214, "model_ms": 6930 },   // the model's work: tokens read/written, time
  "model": "qwen3:4b-instruct",
  "created_at": "2026-10-28T01:45:12.004Z",
  "report_path": "reports/20261028-014512-visionquest-3fa9c1.md",
  "warnings": [                                  // [] when everything went to plan
    "removed 1 link(s) the research never visited (likely made up by the model)"
  ],
  "degraded": false,                             // true = the model couldn't write; markdown is a cited digest of the sources
  "follows": null                                // a follow-up: the id of the report it follows up on
}
```

Markdown conventions:
- Inline citations are links: `[[3]](https://…)`. Numbers match `sources[].n` and run 1, 2, 3… in the order
  the answer cites them (they can differ from the numbers in live `source` events).
- The answer always ends with `---` and a `## Sources` section (cited sources first, then *Further reading*).
- Saved `.md` files additionally start with YAML front-matter (`id`, `query`, `depth`, `intent`, `model`, `created_at`, `duration_ms`, `sources`).

## Endpoints

### `POST /api/research` — blocking

Runs the request and returns the `ResearchResult` when finished. Response header `X-Cookie-Job` carries the job id.
Add `?format=markdown` (or `Accept: text/markdown`) to get just the Markdown as `text/markdown`.

```bash
curl -s localhost:8765/api/research -H 'content-type: application/json' \
  -d '{"query":"VisionQuest"}' | jq -r .markdown

curl -s 'localhost:8765/api/research?format=markdown' -H 'content-type: application/json' \
  -d '{"query":"!deep sodium-ion vs lithium-ion batteries"}' > report.md
```

Status codes: `200` result · `400` invalid request · `401` auth · `502` the research failed (nothing at all found on the web,
the job timed out, or it was cancelled) — body `{"error": "..."}`. Note that a model outage alone does **not** fail
the request: you get `200` with `"degraded": true`.

### `POST /api/research/stream` — Server-Sent Events

Same input; responds `200 text/event-stream` immediately and streams [events](#streaming-events-sse) until `done` or `error`.
If the client disconnects, the job keeps running and is still saved: reconnect with
`GET /api/jobs/{id}/events?from=<last event id>` (the job id is in the `X-Cookie-Job` header and the `started` event).
To stop it, `DELETE /api/jobs/{id}`.

### Idempotency

`POST /api/research`, `/api/research/stream` and `/api/jobs` accept an `Idempotency-Key: <any string ≤200 chars>`
header. A request that repeats the key of a job that is queued, running or done attaches to **that** job instead of
starting a new one — so a supervisor can retry a POST after a network error without doubling the work.
(Keys of failed or cancelled jobs are free to reuse; keys live as long as the job, ~1 hour after it finishes.)

### `POST /api/jobs` — fire and forget

Returns `202 Accepted` with a `Location` header:

```json
{ "id": "job_5c1e…", "query": "…", "status": "queued", "created_at": "…", "events": 1,
  "events_url": "/api/jobs/job_5c1e…/events", "status_url": "/api/jobs/job_5c1e…" }
```

### `GET /api/jobs` · `GET /api/jobs/{id}`

Job snapshot: `status` is `queued` → `running` (⇄ `paused`) → `done` | `error` | `cancelled`. When `done`, the snapshot includes `result`
(a `ResearchResult`, single-job view only) and `report_id`; when `error` or `cancelled`, it includes `error`. Finished jobs
are kept in memory for ~1 hour. Live progress fields (what the website's status view shows):

| field | |
|---|---|
| `stage` | what it is doing now ("Reading the top 2 results", "Writing the answer", …) |
| `depth` | quick / standard / deep, once decided |
| `queued_ms`, `running_ms`, `paused_ms` | time spent waiting for a slot, working (pauses excluded), and paused |
| `position` | place in line (1 = next), while `queued` |
| `pause` | `active`, `pausing` (asked, stops at the next step), `paused`, `resuming` (waiting at the front of the line) |
| `tool_calls`, `sources` | work done so far |
| `report_id`, `warnings`, `degraded` | when done |

### `DELETE /api/jobs/{id}` (or `POST /api/jobs/{id}/cancel`) — cancel

Stops a queued, running or paused job at once and frees its slot; returns the job snapshot (`status: "cancelled"`).
Streams of the job end with `error` `{"message":"cancelled"}`. Cancelling a finished job changes nothing.

### `POST /api/jobs/{id}/pause` · `POST /api/jobs/{id}/resume`

See [the queue](#the-queue-pause-resume-cancel). Both return the job snapshot; `409` if the job is queued (pause)
or already finished.

### `GET /api/jobs/{id}/events` — SSE replay & resume

Streams the job's events, then live events until it finishes. Safe to (re)connect at any time.
By default it starts from the first event; `?from=N` (or a `Last-Event-ID: N` header, which `EventSource`
sends automatically on reconnect) resumes after event `N`.

### Reports

| method & path | returns |
|---------------|---------|
| `GET /api/reports?limit=50` | `{"reports":[{id, query, depth, intent, created_at, sources, words}]}` newest first |
| `GET /api/reports/{id}` | `ResearchResult` (or Markdown with `?format=markdown`) |
| `GET /api/reports/{id}/markdown` | the saved `.md` file (with front-matter), `text/markdown` |
| `DELETE /api/reports/{id}` | `204` (also deletes its stored pages) |
| `POST /api/reports/{id}/ask` | a follow-up — see below |
| `GET /api/reports/{id}/sources` | its numbered sources, with `stored` (full text kept) and image counts |
| `GET /api/reports/{id}/sources/{n}` | the full content of source `n` |
| `GET /api/reports/{id}/sources/{n}/images/{k}` | image `k` of source `n` (bytes) |
| `GET /api/reports/{id}/content` | the content of all its cited/read sources in one document |

### Introspection

| method & path | returns |
|---------------|---------|
| `GET /api/health` | `status` (`ok`/`degraded`), Ollama reachability, model availability & capabilities, installed models, tools, search backends with their health (`search_health`: successes, failures, `cooldown_secs`, `last_error`), running / queued / paused jobs, settings, `uptime_secs` |
| `GET /api/status` | **live status in one call**: `service` (version, pid, uptime), `queue` (slots in use, running / paused / queued counts, limits), `jobs` (active jobs in queue order: running, paused, then queued by `position`), `recent_jobs` (last 10 finished), `model` (main, fast, loaded, memory), `search` (engine health), `latency`, and `watchdog` (its view, if configured) |
| `GET /api/status/stream?interval=2` | the same document pushed every `interval` seconds (1–60) as SSE `status` events — for dashboards |
| `GET /api/manifest` | machine-readable agent card (name, capabilities, input/output, endpoints) and its `contract`: queue and concurrency limits, `job_timeout_secs`, the pause/cancel semantics, and observed `latency` per depth (median and 90th percentile of the last 20 jobs on this machine) |
| `GET /api/tools` | the JSON-schema tool specs the model receives |

## The queue: pause, resume, cancel

One small model serves one request at a time (`COOKIE_MAX_CONCURRENT=1`), so jobs take turns:

- **First come, first served.** A new job waits in line (`status: "queued"`, `position: 1` = next) and starts the
  moment the current one finishes, is cancelled or is paused. Every request type (blocking, stream, job, follow-up)
  goes through the same line.
- **The line has a limit** (`COOKIE_MAX_QUEUE`, default 25). Past it, new requests get `503` with `Retry-After: 30`
  instead of waiting forever.
- **Pause** (`POST /api/jobs/{id}/pause`) a running job: a model call in progress is stopped at once (the request to
  Ollama is dropped, so the CPU is free immediately); other steps stop at their next checkpoint. The job gives up
  its slot, so the next queued job runs meanwhile. `pause` reads `pausing` until it has stopped, then `status`
  is `paused` and a `status` event `"Paused"` is sent.
- **Resume** (`POST /api/jobs/{id}/resume`) puts it at the **front** of the line; it carries on where it stopped.
  A model call that was interrupted is made again — if the answer was being written, a `reset` event tells
  clients to discard the partial text, then it streams again. Time spent paused doesn't count against the job's
  time limits, and `running_ms` / `stats.duration_ms` are working time.
- **Cancel** works in any state, including paused.

The GUI has the same controls: **Pause/Resume** next to Stop while a search runs, and Pause/Resume/Stop for every
job in the status view, with each queued job's place in line.

## Service status (the watchdog)

`cookie-watch` is a separate small service (installed next to cookie-web) whose only job is to know whether
cookie-web is up — so it can answer when cookie-web can't:

- cookie-web **registers** with it on start (pid, version, port; `COOKIE_WATCH_URL`), sends a **heartbeat** every
  5 s, and says **bye** on a clean shutdown;
- the watchdog **checks on its own** every 5 s: does that process still exist (`/proc/<pid>`, and is it still
  cookie-web), and does `/api/health` answer? A process that exists but doesn't answer is *unresponsive*.

`GET /watch/status` (through nginx, or `http://127.0.0.1:8790/watch/status` on the box) — always answers:

```json
{ "service": "cookie-web", "status": "online", "ok": true, "api": "up", "reason": "",
  "since": "2026-10-30T09:12:03Z", "checked_at": "…", "last_ok_at": "…",
  "watchdog": { "uptime_secs": 86400, "interval_secs": 5 } }
```

| `status` | meaning | HTTP |
|---|---|---|
| `online` | running and healthy | 200 |
| `degraded` | answering, but the model isn't ready (Ollama down, model not pulled) | 200 |
| `unresponsive` | the process exists but `/api/health` fails twice in a row | 503 |
| `offline` | not running: exited, crashed, or stopped (`reason` says which) | 503 |
| `unknown` | nothing checked yet (the first seconds after the watchdog starts) | 503 |

So a plain uptime monitor can watch `/watch/status` by status code. Signed in (session cookie) or with the API key,
the response adds `detail` (pid, version, start time, last heartbeat, health-check latency, running/queued/paused
jobs) and `incidents` (the last 50 status changes with their reasons). `/watch/` is a status page for people; when
cookie-web is down, nginx sends browsers there (and returns `503 {"error":"cookie-web is offline"}` to API calls)
instead of a bare 502. The page returns to where you were once cookie-web is back.

On top of that, systemd itself restarts cookie-web if it crashes (`Restart=always`) **or hangs**: the unit is
`Type=notify` with `WatchdogSec=60`, and cookie-web pings systemd only while its own HTTP server answers.

## Follow-ups and source content

A report is not a dead end. Its sources keep their numbers, and their full text is kept with it, so a caller can
come back with counter-questions instead of starting a new search — or simply pull the material itself.

### Asking a follow-up

```bash
# blocking; returns a ResearchResult (?format=markdown works too)
curl -s localhost:8765/api/reports/$ID/ask -H 'content-type: application/json' \
  -d '{"question": "what does @3 say about pricing?"}'
```

Body: `question` (required), `sources` ([int]), `web` (`auto`|`never`|`always`), `depth`, `model`, `save`.
It is the same as `POST /api/research` (or `/stream`, or `/api/jobs`) with `"report": "<id>"`, so streaming, jobs,
cancelling and `Idempotency-Key` all work for follow-ups.

How it is answered:

1. **Which sources** — the ones named (`sources`, or `@3` / `@1,4` in the question); otherwise the report's cited and
   read sources (up to 6).
2. **Their full text** — the copy stored with the report (pages read while researching are stored automatically);
   a source that was only a search result is read now, and stored for next time.
3. **The earlier answer** is given to the model as context, so "explain the second point" works.
4. **More from the web** — with `web: auto`, only when the sources don't cover the question (and never when sources
   are named; a `warning` says so if they look off-topic). The follow-up question is rewritten into a standalone
   search query first ("how old is he?" → "Samuel L. Jackson age").
5. **The answer** cites with the **same numbers** as the earlier report (`[3]` is the same page in both); new
   sources from a web search are numbered after the old ones. `result.follows` holds the parent report id, and
   the follow-up is itself a saved report that can be followed up.

### Getting a source's content

```bash
curl -s "localhost:8765/api/reports/$ID/sources/3"                       # JSON
curl -s "localhost:8765/api/reports/$ID/sources/3?format=text"           # plain text
curl -s "localhost:8765/api/reports/$ID/sources/3?format=markdown&focus=pricing&max_chars=4000"
curl -s "localhost:8765/api/reports/$ID/sources/3/images/1" -o image.jpg
curl -s "localhost:8765/api/reports/$ID/content?format=text"             # all cited/read sources
curl -s "localhost:8765/api/reports/$ID/content?sources=1,3&format=json"
```

| query parameter | meaning |
|-----------------|---------|
| `format` | `json` (default), `markdown` or `text` (also chosen by `Accept: text/markdown` / `text/plain`) |
| `focus` | only the passages relevant to this (keywords or a question) |
| `max_chars` | cap the content (default: the whole page up to 60 000 characters; for `/content`, a share of 120 000 per source) |
| `sources` | `/content` only: which sources (default: the ones the answer cites, plus pages that were read) |

The content is the page's **main text** (navigation, ads and footers removed by the readability extractor), not the
raw HTML. JSON for one source:

```jsonc
{
  "report": "20261028-…", "n": 3,
  "title": "…", "url": "https://…",            // the source as cited
  "read_from": "https://…",                    // where it was actually read (e.g. a Wayback Machine copy)
  "published": "2026-10-01", "byline": "…", "site_name": "…",
  "words": 1834, "truncated": false,
  "content": "…Markdown…",
  "images": [ { "k": 1, "url": "https://…/lead.jpg", "alt": "", "proxy": "/api/reports/…/sources/3/images/1" } ]
}
```

Images are the page's lead image (`og:image`) and the images inside its main content (lazy-loading placeholders
resolved, icons and tracking pixels skipped). The `proxy` URL fetches the bytes through cookie-web (useful when
the caller has no internet access of its own); only `image/*` responses are passed through.

Errors: unknown report or source → `404`; a page that can't be read now → `502`.

## Streaming events (SSE)

Each event is sent as

```
event: <type>
id: <sequence number, from 1>
data: <JSON object with a "type" field equal to <type>>
```

Keep-alive comments are sent every 15 s.

| type | data fields | notes |
|------|-------------|-------|
| `started` | `job_id`, `query` | always first |
| `triage` | `depth`, `intent`, `search_query`, `reason`, `decided_by` | |
| `status` | `message` | human-readable progress (`"Paused"` / `"Resumed"` when the job is paused and resumed) |
| `tool_call` | `call_id`, `tool`, `args` | `call_id` pairs with `tool_result` |
| `tool_result` | `call_id`, `tool`, `ok`, `summary` | |
| `source` | `source` (a source object) | sent whenever a source is added/updated; upsert by `source.n` |
| `token` | `text` | a chunk of the final Markdown as it is written (before citation linking) |
| `reset` | `reason` | discard the `token` preview so far: a writing attempt failed and the answer is being rewritten |
| `warning` | `message` | something went wrong but the job carries on (also collected in `result.warnings`) |
| `done` | `result` (`ResearchResult`) | final; its `markdown` is the authoritative, citation-linked version |
| `error` | `message` | final; no `done` follows |

> Render `token` chunks for a live preview, then replace the preview with `done.result.markdown`.

## Errors

Every error body is `{"error": "<message>"}`. Unknown routes → `404`. `409`: pause/resume not possible in the job's
state. `503` + `Retry-After`: the queue is full (or, through nginx, cookie-web is down — see `/watch/status`).

## Reliability guarantees

What a caller can count on:

- **Every job ends** with exactly one `done` or `error` event — also when it is cancelled, times out
  (`COOKIE_JOB_TIMEOUT_SECS`, default 30 min) or crashes internally.
- **A model outage is not a failure.** Ollama calls are retried with back-off; if the writer still fails, the
  result is a cited digest of the sources with `"degraded": true` and an explanation in `warnings`.
- **Empty or off-topic search results are rescued**: the request is rephrased (by the model, or mechanically if
  the model is down) and searched again before anything is written.
- **Search keeps working when engines don't**: backends that fail are rested on a growing cooldown, the others
  fill in, and when every backend fails the search is retried once.
- **Dead links don't cost anything**: failed page reads don't use up the page budget, and dead or blocked pages are
  read from the Wayback Machine (`COOKIE_ARCHIVE_FALLBACK`).
- **No invented sources**: citations to sources the writer wasn't shown and Markdown links to URLs the research never
  visited are removed (and reported in `warnings`).
- **Fact-checked names**: statements that connect names (who plays whom, who founded what) are checked against the
  source sentences; ones the sources contradict or don't contain are removed and quoted in `warnings`.
- **Time-boxed research**: each depth has a time budget (scaled by `COOKIE_TIME_SCALE`); when it runs out the answer is
  written from what was found.
- **Retries are safe** with `Idempotency-Key`; **disconnects are recoverable** with `?from=` / `Last-Event-ID`.

Only two things fail a request outright: nothing at all could be found on the web (even after rephrasing), or
the hard time limit was hit.

## Client examples

**Python (requests, SSE)**

```python
import json, requests

with requests.post("http://localhost:8765/api/research/stream",
                   json={"query": "how do I set up WireGuard on Ubuntu?"}, stream=True) as r:
    event = None
    for line in r.iter_lines(decode_unicode=True):
        if line.startswith("event:"):
            event = line[6:].strip()
        elif line.startswith("data:"):
            data = json.loads(line[5:])
            if event == "token":
                print(data["text"], end="", flush=True)
            elif event == "done":
                open("answer.md", "w").write(data["result"]["markdown"])
            elif event == "error":
                raise RuntimeError(data["message"])
```

**JavaScript (fetch, works in browsers and Node 18+)**

```js
const res = await fetch("http://localhost:8765/api/research", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ query: "VisionQuest" }),
});
const { markdown, sources } = await res.json();
```

**Rust (reqwest)**

```rust
let result: serde_json::Value = reqwest::Client::new()
    .post("http://localhost:8765/api/research")
    .json(&serde_json::json!({ "query": "VisionQuest", "depth": "quick" }))
    .send().await?.json().await?;
println!("{}", result["markdown"].as_str().unwrap_or_default());
```

**Async job + polling (shell)**

```bash
JOB=$(curl -s localhost:8765/api/jobs -H 'content-type: application/json' -d '{"query":"!deep state of small LLMs"}' | jq -r .id)
until [ "$(curl -s localhost:8765/api/jobs/$JOB | jq -r .status)" = done ]; do sleep 5; done
curl -s localhost:8765/api/jobs/$JOB | jq -r .result.markdown
```

## Integrating with a supervisor

cookie-web is designed to be one specialist in a larger system:

1. **Discover** — `GET /api/manifest` describes what it does, its input and output, its endpoints, and its
   `contract` (limits, queueing, time bound, typical latency per depth on this box — use it to set your own timeouts).
2. **Check** — `GET /watch/status` (always answers, cheap) → route elsewhere if `api != "up"`; `GET /api/health`
   for the details.
3. **Delegate** — `POST /api/jobs` for long work, or `/api/research` when the caller can block.
   Pass `depth` explicitly when the supervisor already knows how much it needs, and an `Idempotency-Key`
   (e.g. the supervisor's task id) so retries never start duplicate work.
4. **Observe** — subscribe to `/api/jobs/{id}/events` to relay progress; resume with `?from=` after a disconnect.
   `DELETE /api/jobs/{id}` when the answer is no longer needed; `POST …/pause` to make room for something urgent,
   `…/resume` afterwards.
5. **Consume** — use `result.markdown` for humans and `result.sources` (structured, numbered) for machines.
   Check `result.degraded` (and `warnings`) if the supervisor wants to retry later or ask another agent.
6. **Come back** — counter-questions go to `POST /api/reports/{id}/ask` (with `@n` for a specific source), and the
   raw material is at `/api/reports/{id}/sources/{n}` and `/content`. No need to search again or to ask another agent
   to fetch a link cookie-web already read.

Concurrency: at most `COOKIE_MAX_CONCURRENT` jobs run at once (default 1 — one small model serves one request at a
time); others wait in `queued`, first come first served, up to `COOKIE_MAX_QUEUE`. Treat cookie-web as a service with
a contract, not an agent to babysit: give it a request with the depth you need, and get back one result (or one error)
within the stated time bound.
