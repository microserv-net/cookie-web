#!/usr/bin/env bash
# Install Ollama on Ubuntu/Debian, tune it for a dedicated cookie-web box, and pull the models.
#
#   ./scripts/install-ollama.sh                       # Ollama + qwen3:4b-instruct
#   ./scripts/install-ollama.sh --model qwen3:8b      # a different main model
#   ./scripts/install-ollama.sh --fast-model qwen3:1.7b
#   curl -fsSL https://raw.githubusercontent.com/microserv-net/cookie-web/main/scripts/install-ollama.sh | bash
#
# Safe to run again: it skips what is already done, resumes interrupted downloads and re-applies
# the tuning. Run as a normal user with sudo rights, or as root.
set -euo pipefail

MODEL="${COOKIE_MODEL:-qwen3:4b-instruct}"
FAST_MODEL="${COOKIE_FAST_MODEL:-}"
LISTEN="127.0.0.1:11434"
KV_CACHE="q8_0"
UPGRADE=0
TUNE=1
SMOKE_TEST=1
FORCE=0

usage() {
	cat <<'EOF'
Usage: install-ollama.sh [options]

  --model NAME        main model to pull (default: $COOKIE_MODEL or qwen3:4b-instruct)
  --fast-model NAME   optional second, smaller model for triage/notes (default: none)
  --listen ADDR       where Ollama listens (default 127.0.0.1:11434 — private to this box)
  --kv-cache TYPE     q8_0 (default, half the memory) or f16
  --upgrade           reinstall/upgrade Ollama even if it is already installed
  --no-tune           don't install the systemd tuning drop-in
  --no-test           skip the final test generation
  --force             continue even if the machine looks too small
  -h, --help          this help
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--model) MODEL="${2:?--model needs a value}"; shift 2 ;;
	--fast-model) FAST_MODEL="${2:?--fast-model needs a value}"; shift 2 ;;
	--listen) LISTEN="${2:?--listen needs a value}"; shift 2 ;;
	--kv-cache) KV_CACHE="${2:?--kv-cache needs a value}"; shift 2 ;;
	--upgrade) UPGRADE=1; shift ;;
	--no-tune) TUNE=0; shift ;;
	--no-test) SMOKE_TEST=0; shift ;;
	--force) FORCE=1; shift ;;
	-h | --help) usage; exit 0 ;;
	*) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
	esac
done

case "$KV_CACHE" in q8_0 | q4_0 | f16) ;; *) echo "--kv-cache must be q8_0, q4_0 or f16" >&2; exit 2 ;; esac

# ------------------------------------------------------------------ helpers
if [ -t 1 ]; then B=$'\e[1m' G=$'\e[32m' Y=$'\e[33m' R=$'\e[31m' N=$'\e[0m'; else B='' G='' Y='' R='' N=''; fi
step() { echo; echo "${B}==> $*${N}"; }
ok() { echo "  ${G}✓${N} $*"; }
warn() { echo "  ${Y}!${N} $*"; }
die() { echo "  ${R}✗ $*${N}" >&2; exit 1; }

if [ "$(id -u)" -eq 0 ]; then
	SUDO=""
elif command -v sudo >/dev/null; then
	SUDO="sudo"
	sudo -v || die "sudo is required"
else
	die "run as root or install sudo"
fi

API="http://${LISTEN/0.0.0.0/127.0.0.1}"
# The ollama CLI talks to whatever OLLAMA_HOST says; point it at the server we manage.
export OLLAMA_HOST="$LISTEN"

systemd_running() {
	command -v systemctl >/dev/null || return 1
	case "$(systemctl is-system-running 2>/dev/null || true)" in
	running | degraded | starting) return 0 ;;
	*) return 1 ;;
	esac
}

api_up() { curl -fsS --max-time 3 "$API/api/tags" >/dev/null 2>&1; }

wait_for_api() {
	local secs="${1:-60}"
	for _ in $(seq 1 "$secs"); do
		api_up && return 0
		sleep 1
	done
	return 1
}

# ----------------------------------------------------------- 1. the machine
step "Checking this machine"
. /etc/os-release 2>/dev/null || true
case "${ID:-}${ID_LIKE:-}" in
*ubuntu* | *debian*) ok "${PRETTY_NAME:-Linux}" ;;
*) warn "not Ubuntu/Debian (${PRETTY_NAME:-unknown}); continuing, but this script is tested on Ubuntu" ;;
esac

ARCH="$(uname -m)"
case "$ARCH" in
x86_64)
	if grep -qw avx2 /proc/cpuinfo; then
		ok "CPU: $(nproc) cores, AVX2"
	else
		warn "CPU has no AVX2: Ollama will run, but several times slower"
	fi
	;;
aarch64 | arm64) ok "CPU: $(nproc) ARM cores" ;;
*) die "unsupported CPU architecture: $ARCH" ;;
esac

MEM_GB=$(awk '/MemTotal/ {printf "%.1f", $2/1024/1024}' /proc/meminfo)
if awk -v m="$MEM_GB" 'BEGIN {exit !(m < 4)}'; then
	[ "$FORCE" -eq 1 ] || die "only ${MEM_GB} GB RAM: the model needs ~3.5 GB on its own (use --force to try anyway)"
	warn "only ${MEM_GB} GB RAM — expect swapping"
elif awk -v m="$MEM_GB" 'BEGIN {exit !(m < 7)}'; then
	warn "${MEM_GB} GB RAM: works with the default model, but leaves little headroom (8 GB recommended)"
else
	ok "RAM: ${MEM_GB} GB"
fi

FREE_GB=$(df -Pk / | awk 'NR==2 {printf "%.0f", $4/1024/1024}')
if [ "$FREE_GB" -lt 6 ]; then
	[ "$FORCE" -eq 1 ] || die "only ${FREE_GB} GB free on /: Ollama (~1.5 GB) and the model (~2.5 GB) need about 6 GB"
	warn "only ${FREE_GB} GB free on /"
else
	ok "disk: ${FREE_GB} GB free"
fi

if command -v nvidia-smi >/dev/null && nvidia-smi -L >/dev/null 2>&1; then
	ok "NVIDIA GPU: $(nvidia-smi -L | head -1)"
fi

# --------------------------------------------------------- 2. prerequisites
step "Installing prerequisites"
NEED=()
for pkg in curl ca-certificates zstd pciutils; do
	dpkg -s "$pkg" >/dev/null 2>&1 || NEED+=("$pkg")
done
if [ ${#NEED[@]} -gt 0 ]; then
	if command -v apt-get >/dev/null; then
		$SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -qq
		$SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${NEED[@]}" >/dev/null
		ok "installed: ${NEED[*]}"
	else
		die "please install: ${NEED[*]}"
	fi
else
	ok "curl, zstd and pciutils are present"
fi

# ---------------------------------------------------------------- 3. Ollama
step "Installing Ollama"
if command -v ollama >/dev/null && [ "$UPGRADE" -eq 0 ]; then
	ok "already installed: $(ollama --version 2>/dev/null | tail -1) (use --upgrade to update)"
else
	TMP="$(mktemp -d)"
	trap 'rm -rf "$TMP"' EXIT
	for attempt in 1 2 3; do
		if curl -fsSL --retry 3 https://ollama.com/install.sh -o "$TMP/install.sh"; then break; fi
		[ "$attempt" -eq 3 ] && die "could not download the Ollama installer (network?)"
		sleep $((attempt * 5))
	done
	# The official installer downloads ~1.5 GB and sets up the `ollama` systemd service.
	sh "$TMP/install.sh" || die "the Ollama installer failed (see above)"
	command -v ollama >/dev/null || die "ollama is not on PATH after installing"
	ok "installed: $(ollama --version 2>/dev/null | tail -1)"
fi

# ---------------------------------------------------------- 4. tuning + start
step "Configuring and starting Ollama"
OLLAMA_ENV=(
	"OLLAMA_HOST=$LISTEN"
	"OLLAMA_KEEP_ALIVE=-1"
	"OLLAMA_NUM_PARALLEL=1"
	"OLLAMA_MAX_LOADED_MODELS=1"
	"OLLAMA_FLASH_ATTENTION=1"
	"OLLAMA_KV_CACHE_TYPE=$KV_CACHE"
)
if systemd_running; then
	if [ "$TUNE" -eq 1 ]; then
		# A drop-in, not an edit of ollama.service: Ollama's installer rewrites that file on upgrade.
		$SUDO mkdir -p /etc/systemd/system/ollama.service.d
		{
			echo "# Written by cookie-web/scripts/install-ollama.sh — one model, always loaded, half-size KV cache."
			echo "[Service]"
			for kv in "${OLLAMA_ENV[@]}"; do echo "Environment=\"$kv\""; done
			echo "# The model uses every core while it answers; lower priority keeps SSH and cookie-web snappy"
			echo "# (it still gets all the CPU nothing else wants, so answers are not slower on an idle box)."
			echo "Nice=10"
			echo "CPUWeight=50"
		} | $SUDO tee /etc/systemd/system/ollama.service.d/cookie-web.conf >/dev/null
		ok "tuning: /etc/systemd/system/ollama.service.d/cookie-web.conf"
	fi
	$SUDO systemctl daemon-reload
	$SUDO systemctl enable --quiet ollama
	$SUDO systemctl restart ollama
	wait_for_api 90 || die "Ollama did not come up — check: journalctl -u ollama -n 50"
	ok "service running (starts on boot): $API"
else
	# Containers, WSL without systemd, …: run it in the background ourselves.
	warn "systemd is not running here, so Ollama won't start on boot"
	if api_up; then
		ok "Ollama is already running at $API"
	else
		LOG=/tmp/ollama-serve.log
		if [ "$TUNE" -eq 1 ]; then
			nohup nice -n 10 env "${OLLAMA_ENV[@]}" ollama serve >"$LOG" 2>&1 &
		else
			nohup env "OLLAMA_HOST=$LISTEN" ollama serve >"$LOG" 2>&1 &
		fi
		wait_for_api 60 || die "Ollama did not come up — see $LOG"
		ok "started in the background (log: $LOG)"
	fi
fi

# ---------------------------------------------------------------- 5. models
pull() {
	local m="$1"
	if ollama list 2>/dev/null | awk 'NR>1 {print $1}' | grep -qx -e "$m" -e "$m:latest"; then
		ok "$m is already pulled"
		return 0
	fi
	for attempt in 1 2 3 4 5; do
		# `ollama pull` resumes a partial download, so retrying is cheap.
		if ollama pull "$m"; then
			ok "pulled $m"
			return 0
		fi
		warn "pull failed (attempt $attempt/5); retrying in $((attempt * 10))s"
		sleep $((attempt * 10))
	done
	die "could not pull $m — check the name at https://ollama.com/library"
}

has_tools() {
	curl -fsS --max-time 30 "$API/api/show" -d "{\"model\":\"$1\"}" | grep -q '"tools"'
}

step "Pulling models"
MODELS=("$MODEL")
[ -n "$FAST_MODEL" ] && [ "$FAST_MODEL" != "$MODEL" ] && MODELS+=("$FAST_MODEL")
for m in "${MODELS[@]}"; do
	pull "$m"
	if has_tools "$m"; then
		ok "$m supports native tool calling"
	else
		warn "$m has no native tool calling: cookie-web will use its JSON fallback (works, less smoothly)"
	fi
done

# ------------------------------------------------------------------ 6. test
if [ "$SMOKE_TEST" -eq 1 ]; then
	step "Test generation with $MODEL (loads it into memory; can take a minute on a CPU)"
	START=$(date +%s)
	REPLY_JSON=$(curl -fsS --max-time 900 "$API/api/chat" -d "{
		\"model\": \"$MODEL\",
		\"messages\": [{\"role\": \"user\", \"content\": \"Reply with just the word: ready\"}],
		\"stream\": false,
		\"keep_alive\": -1,
		\"options\": {\"num_predict\": 8, \"num_ctx\": 8192}
	}") || die "the model did not answer — check the Ollama log"
	REPLY=$(printf '%s' "$REPLY_JSON" | sed -n 's/.*"content": *"\([^"]*\)".*/\1/p' | head -c 60)
	ok "answered \"${REPLY}\" in $(($(date +%s) - START))s"
	USED=$(curl -fsS "$API/api/ps" | sed -n 's/.*"size": *\([0-9]*\).*/\1/p' | head -1)
	[ -n "$USED" ] && ok "loaded model uses $(awk -v b="$USED" 'BEGIN {printf "%.2f", b/1e9}') GB"
fi

step "Done"
echo "  Ollama:  $API   models: ${MODELS[*]}"
echo "  Next:    ./scripts/install-cookie-web.sh   (or: COOKIE_MODEL=$MODEL cargo run --release)"
