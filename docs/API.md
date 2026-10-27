# cookie-web HTTP API

Base URL: `http://<COOKIE_HOST>:<COOKIE_PORT>` (default `http://127.0.0.1:8765`).
All request/response bodies are JSON (UTF-8) unless noted. The GUI at `/` uses exactly these endpoints.

- [Authentication](#authentication)
- [The request object](#the-request-object)
- [The result object](#the-result-object)
- [Endpoints](#endpoints)
- [Streaming events (SSE)](#streaming-events-sse)
- [Errors](#errors)
- [Client examples](#client-examples)
- [Integrating with a supervisor](#integrating-with-a-supervisor)

---

## Authentication

Off by default. Set `COOKIE_API_KEY=<secret>` to require a key on every `/api/*` route except
`/api/health` and `/api/manifest`. Send it as any of:

```
Authorization: Bearer <secret>
X-API-Key: <secret>
?api_key=<secret>          # query param, for EventSource clients that can't set headers
```

Missing/wrong key → `401 {"error":"missing or invalid API key"}`.

## The request object

`POST` bodies for `/api/research`, `/api/research/stream` and `/api/jobs`:

| field   | type    | default | meaning |
|---------|---------|---------|---------|
| `query` | string  | —       | **Required.** Natural-language request (1–4000 chars). A leading `!quick`, `!standard` or `!deep` forces the depth. |
| `depth` | string  | `auto`  | `auto`, `quick`, `standard` or `deep`. Anything else → 400. |
| `model` | string  | server default | Ollama model for the agent loop and the writer (e.g. `qwen3:4b`). |
| `save`  | bool    | `true`  | Persist the report (`.md` + `.json`) to `COOKIE_REPORTS_DIR`. |

```json
{ "query": "VisionQuest", "depth": "auto" }
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
  "stats": { "tool_calls": 2, "searches": 1, "pages_read": 0, "llm_calls": 1, "duration_ms": 8421 },
  "model": "qwen3:8b",
  "created_at": "2026-10-28T01:45:12.004Z",
  "report_path": "reports/20261028-014512-visionquest-3fa9c1.md"
}
```

Markdown conventions:
- Inline citations are links: `[[3]](https://…)`. Numbers match `sources[].n`.
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

Status codes: `200` result · `400` invalid request · `401` auth · `502` the research failed (e.g. Ollama down, all search backends failing) — body `{"error": "..."}`.

### `POST /api/research/stream` — Server-Sent Events

Same input; responds `200 text/event-stream` immediately and streams [events](#streaming-events-sse) until `done` or `error`.
If the client disconnects, the job keeps running and is still saved.

### `POST /api/jobs` — fire and forget

Returns `202 Accepted` with a `Location` header:

```json
{ "id": "job_5c1e…", "query": "…", "status": "queued", "created_at": "…", "events": 1,
  "events_url": "/api/jobs/job_5c1e…/events", "status_url": "/api/jobs/job_5c1e…" }
```

### `GET /api/jobs` · `GET /api/jobs/{id}`

Job snapshot: `status` is `queued` → `running` → `done` | `error`. When `done`, the snapshot includes `result`
(a `ResearchResult`); when `error`, it includes `error`. Finished jobs are kept in memory for ~1 hour.

### `GET /api/jobs/{id}/events` — SSE replay

Streams every event of the job from the beginning, then live events until it finishes. Safe to (re)connect at any time.

### Reports

| method & path | returns |
|---------------|---------|
| `GET /api/reports?limit=50` | `{"reports":[{id, query, depth, intent, created_at, sources, words}]}` newest first |
| `GET /api/reports/{id}` | `ResearchResult` (or Markdown with `?format=markdown`) |
| `GET /api/reports/{id}/markdown` | the saved `.md` file (with front-matter), `text/markdown` |
| `DELETE /api/reports/{id}` | `204` |

### Introspection

| method & path | returns |
|---------------|---------|
| `GET /api/health` | `status` (`ok`/`degraded`), Ollama reachability, model availability & capabilities, installed models, tools, search backends, running jobs |
| `GET /api/manifest` | machine-readable agent card (name, capabilities, input/output, endpoints) for orchestrators |
| `GET /api/tools` | the JSON-schema tool specs the model receives |

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
| `status` | `message` | human-readable progress |
| `tool_call` | `call_id`, `tool`, `args` | `call_id` pairs with `tool_result` |
| `tool_result` | `call_id`, `tool`, `ok`, `summary` | |
| `source` | `source` (a source object) | sent whenever a source is added/updated; upsert by `source.n` |
| `token` | `text` | a chunk of the final Markdown as it is written (before citation linking) |
| `done` | `result` (`ResearchResult`) | final; its `markdown` is the authoritative, citation-linked version |
| `error` | `message` | final; no `done` follows |

> Render `token` chunks for a live preview, then replace the preview with `done.result.markdown`.

## Errors

Every error body is `{"error": "<message>"}`. Unknown routes → `404`.

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

1. **Discover** — `GET /api/manifest` describes what it does, its input and output, and its endpoints.
2. **Check** — `GET /api/health` → route elsewhere if `status != "ok"`.
3. **Delegate** — `POST /api/jobs` for long work, or `/api/research` when the caller can block.
   Pass `depth` explicitly when the supervisor already knows how much it needs.
4. **Observe** — subscribe to `/api/jobs/{id}/events` to relay progress.
5. **Consume** — use `result.markdown` for humans and `result.sources` (structured, numbered) for machines.

Concurrency: at most `COOKIE_MAX_CONCURRENT` jobs run at once (default 2); others wait in `queued`.
