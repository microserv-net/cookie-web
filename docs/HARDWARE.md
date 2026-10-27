# Model choice & hardware sizing

cookie-web is meant to run **alone on a small, cheap Linux box**, always loaded, answering a supervisor
over the network without borrowing the supervisor's memory. This page explains which model it uses,
why, and what hardware that needs.

## The model: `qwen3:4b-instruct` (Qwen3-4B-Instruct-2507, Q4_K_M, 2.5 GB)

The model's jobs in cookie-web, from easiest to hardest:

1. **Triage** — mostly handled by heuristics; the model only classifies unclear requests (tiny JSON).
2. **Note-taking** — extract facts from one page (deep mode). Easy summarisation.
3. **Choosing tool calls** — which page to read, what to search next. Needs reliable tool calling.
4. **Writing** — a faithful, well-formatted answer with correct `[n]` citations. The hardest part.

Steps 3–4 set the floor. Measured tool-calling accuracy drops sharply below ~4B parameters:

| model | size (Ollama, Q4) | tool calling | verdict |
|-------|------------------|--------------|---------|
| **qwen3:4b-instruct** (2507) | **2.5 GB** | ~87 on a validated BFCL slice (frontier ≈ 99); the 4-bit build ~83 | **default** — smallest reliable tool-caller, non-thinking (no wasted tokens), Apache-2.0 |
| qwen3:1.7b | 1.4 GB | ~56 on the same slice; rated unreliable | budget fallback, **pipeline mode only**, noticeably weaker writing |
| qwen3:8b | 5.2 GB | better | ~2× the RAM and ~2× slower on CPU for a modest gain here |
| qwen3.5:4b | larger (multimodal) | BFCL-v4 ~0.50 | no advantage for a text-only agent |
| gemma4:e2b | 7.2 GB download | BFCL-v4 ~37 | heavier and weaker at tools than it sounds |
| LFM2.5-2.6B | ~2.5 GB | strong tool scores | not in the Ollama library, restrictive licence, early reports of confident errors — worth re-testing later |

> **Watch the tag:** plain `qwen3:4b` is the *thinking* variant — it spends hundreds of tokens
> reasoning before every answer, which is slow on CPU. Use `qwen3:4b-instruct`.

cookie-web also does the things small models are bad at **in code**: it seeds the first search, budgets
and de-duplicates tool calls, guarantees pages actually get read, excerpts pages to the relevant passages,
checks citations, and fits every prompt to the context window. That is why a 4B model is enough.

## Memory, always loaded (8K context — the default)

| component | memory | how it's derived |
|-----------|--------|------------------|
| model weights | **2.5 GB** | `qwen3:4b-instruct` Q4_K_M |
| KV cache, 8192 tokens | **0.64 GB** with `OLLAMA_KV_CACHE_TYPE=q8_0` (1.21 GB at f16) | 36 layers × 8 KV heads × 128 dims × 2 (K+V) = 147 KB/token at f16 |
| compute buffers / runtime | ~0.3–0.5 GB | estimate |
| **Ollama total** | **≈ 3.5 GB** (≈ 4.1 GB at f16 KV) | `cookie-web doctor` prints the real figure once loaded |
| cookie-web | **~16 MB** RSS | measured (release build, 16 MB binary) |
| SearXNG (optional, same box) | ~0.2–0.3 GB | |
| Linux server OS (no desktop) | ~0.3–0.5 GB | |
| **Total** | **≈ 4.2–4.8 GB** | |

Why 8K context: every prompt cookie-web builds is sized to fit it (evidence budgets, page excerpts,
compaction of the agent conversation). 16K buys more page text per answer for another ~0.6 GB
(q8_0 KV) — set `COOKIE_NUM_CTX=16384` on 16 GB boxes.

## Minimum system requirements (the benchmark)

| | minimum | recommended |
|---|---|---|
| RAM | **8 GB** (6 GB is the hard floor, no headroom) | 16 GB dual-channel (16K context, or room for a second service) |
| CPU | x86-64 with **AVX2**, 4 cores (Intel N100/N150 class) | 6–8 core Zen 3+ / recent Core i5 (≈2–3× faster prompt processing) |
| Storage | 32 GB SSD (model 2.5 GB + OS) | 64 GB+ NVMe |
| GPU | none needed | any 6 GB+ GPU makes it several times faster |
| Network | Ethernet, outbound HTTPS | |
| OS | 64-bit Linux (Debian/Ubuntu Server) | |

ARM boxes (e.g. Raspberry Pi 5 8 GB) also run it, slower.

### What speed to expect (CPU-only, rough)

On CPU the cost is dominated by **reading prompts** (prefill), then by writing. Token generation is roughly
bounded by memory bandwidth ÷ model size, so dual-channel RAM matters more than clock speed.
Ballpark for an N100-class box: quick answers in about a minute, standard answers in a few minutes, deep
reports in 10–20 minutes. Faster CPUs or any GPU cut that substantially. These are estimates —
time `cookie-web ask "…"` on the actual box.

To make it faster on weak hardware without changing the model:
- `COOKIE_AGENT_MODE=pipeline` — skip the model's extra tool rounds (code still searches and reads the top
  results). Fewer LLM calls, slightly less adaptive research.
- Send `"depth":"quick"` or `"standard"` from the supervisor when you don't need a report.

## Ollama settings for a dedicated box

Set these on the Ollama service (`sudo systemctl edit ollama`, see [`deploy/ollama.conf`](../deploy/ollama.conf)):

```ini
[Service]
Environment="OLLAMA_KEEP_ALIVE=-1"          # never unload the model
Environment="OLLAMA_NUM_PARALLEL=1"         # one KV cache, not four
Environment="OLLAMA_MAX_LOADED_MODELS=1"
Environment="OLLAMA_FLASH_ATTENTION=1"      # required for the quantised KV cache
Environment="OLLAMA_KV_CACHE_TYPE=q8_0"     # halves KV memory, negligible quality loss
Environment="OLLAMA_HOST=127.0.0.1:11434"   # keep Ollama private; only cookie-web is exposed
```

cookie-web itself already defaults to `COOKIE_KEEP_ALIVE=-1`, `COOKIE_NUM_CTX=8192`,
`COOKIE_MAX_CONCURRENT=1` and loads the model when it starts (`COOKIE_WARMUP=true`), so the first request
doesn't pay the load time.

## Bare-metal install (no Docker)

On Ubuntu/Debian the scripts do all of this (and are safe to re-run):

```bash
./scripts/setup-ubuntu.sh                       # = install-ollama.sh + install-cookie-web.sh
./scripts/install-ollama.sh --model qwen3:4b-instruct --fast-model qwen3:1.7b
./scripts/install-cookie-web.sh --binary ./cookie-web    # binary built on a faster machine
```

`install-ollama.sh` writes the tuning above as a systemd drop-in
(`/etc/systemd/system/ollama.service.d/cookie-web.conf`), so Ollama upgrades don't undo it. Compiling cookie-web
takes a while on a 4-core mini PC; building once elsewhere (same CPU architecture) and passing `--binary` is quicker.

By hand, the same steps are:

```bash
# 1. Ollama + model
curl -fsSL https://ollama.com/install.sh | sh
sudo systemctl edit ollama            # paste deploy/ollama.conf
sudo systemctl restart ollama
ollama pull qwen3:4b-instruct

# 2. cookie-web (build on any machine with Rust, copy the 16 MB binary over)
cargo build --release
sudo install -m755 target/release/cookie-web /usr/local/bin/
sudo useradd -r -m -d /var/lib/cookie-web cookie
sudo cp deploy/cookie-web.env /etc/cookie-web.env    # set COOKIE_API_KEY!
sudo cp deploy/cookie-web.service /etc/systemd/system/
sudo systemctl enable --now cookie-web
cookie-web doctor                     # shows the loaded model's real memory use
```

Because the box is exposed to the network, set `COOKIE_API_KEY` and give the supervisor that key.
