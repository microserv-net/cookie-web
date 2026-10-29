#!/usr/bin/env bash
# Build cookie-web and install it as a systemd service on Ubuntu/Debian.
# Run from a checkout of the repository, after scripts/install-ollama.sh:
#
#   ./scripts/install-cookie-web.sh                        # build here, serve on 0.0.0.0:8765 with an API key
#   ./scripts/install-cookie-web.sh --local                # only reachable from this machine, no key
#   ./scripts/install-cookie-web.sh --binary ./cookie-web  # use a binary built on another machine
#
# Safe to run again (upgrades the binary, keeps your settings and API key).
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODEL="${COOKIE_MODEL:-}"
FAST_MODEL="${COOKIE_FAST_MODEL:-}"
PORT=""
HOST=""
API_KEY=""
PASSWORD=""
NO_KEY=0
BINARY=""
OPEN_FIREWALL=0
OLLAMA_URL=""
THREADS=""

usage() {
	cat <<'EOF'
Usage: install-cookie-web.sh [options]

  --model NAME        Ollama model cookie-web uses (default: qwen3:4b-instruct)
  --fast-model NAME   small model for the quick structured jobs (default qwen3:1.7b; "none" = main model only)
  --port N            port to serve on (default 8765)
  --local             listen on 127.0.0.1 only, without an API key
  --api-key KEY       use this API key (default: keep the existing one, or generate one)
  --password PW       website password (default: the API key signs you in too)
  --no-key            no API key (anyone on the network can use it — only on trusted networks)
  --binary PATH       install this prebuilt binary instead of compiling (compiling on a small box is slow)
  --ollama URL        where Ollama runs (default http://127.0.0.1:11434)
  --threads N         CPU threads the model may use (default: all cores; e.g. cores-1 keeps one free)
  --open-firewall     allow the port through ufw if ufw is active
  -h, --help          this help
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--model) MODEL="${2:?}"; shift 2 ;;
	--fast-model) FAST_MODEL="${2:?}"; shift 2 ;;
	--port) PORT="${2:?}"; shift 2 ;;
	--local) HOST="127.0.0.1"; NO_KEY=1; shift ;;
	--api-key) API_KEY="${2:?}"; shift 2 ;;
	--password) PASSWORD="${2:?}"; shift 2 ;;
	--no-key) NO_KEY=1; shift ;;
	--binary) BINARY="${2:?}"; shift 2 ;;
	--ollama) OLLAMA_URL="${2:?}"; shift 2 ;;
	--threads) THREADS="${2:?}"; shift 2 ;;
	--open-firewall) OPEN_FIREWALL=1; shift ;;
	-h | --help) usage; exit 0 ;;
	*) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
	esac
done

if [ -t 1 ]; then B=$'\e[1m' G=$'\e[32m' Y=$'\e[33m' R=$'\e[31m' N=$'\e[0m'; else B='' G='' Y='' R='' N=''; fi
step() { echo; echo "${B}==> $*${N}"; }
ok() { echo "  ${G}✓${N} $*"; }
warn() { echo "  ${Y}!${N} $*"; }
die() { echo "  ${R}✗ $*${N}" >&2; exit 1; }

if [ "$(id -u)" -eq 0 ]; then SUDO=""; else SUDO="sudo"; sudo -v || die "sudo is required"; fi

# Build as the real user even under sudo, so target/ and ~/.cargo don't end up owned by root.
BUILD_USER="${SUDO_USER:-$(id -un)}"
as_builder() {
	if [ "$(id -un)" = "$BUILD_USER" ]; then "$@"; else sudo -u "$BUILD_USER" -H "$@"; fi
}

systemd_running() {
	command -v systemctl >/dev/null || return 1
	case "$(systemctl is-system-running 2>/dev/null || true)" in running | degraded | starting) return 0 ;; *) return 1 ;; esac
}

ENV_FILE=/etc/cookie-web.env
DATA=/var/lib/cookie-web

# Set KEY=VALUE in the env file (replacing an existing, possibly commented-out, line).
set_env() {
	local key="$1" value="$2" esc
	# escape what sed treats specially in a replacement (passwords may contain anything)
	esc="$(printf '%s' "$value" | sed -e 's/[\\|&]/\\&/g')"
	if $SUDO grep -qE "^#? *${key}=" "$ENV_FILE"; then
		$SUDO sed -i -E "s|^#? *${key}=.*|${key}=${esc}|" "$ENV_FILE"
	else
		echo "${key}=${value}" | $SUDO tee -a "$ENV_FILE" >/dev/null
	fi
}
get_env() { $SUDO sed -n -E "s/^${1}=(.*)/\1/p" "$ENV_FILE" 2>/dev/null | tail -1; }

# ------------------------------------------------------------------ 1. build
step "Getting the cookie-web binary"
if [ -n "$BINARY" ]; then
	[ -x "$BINARY" ] || die "$BINARY is not an executable file"
	"$BINARY" --version >/dev/null || die "$BINARY does not run on this machine (built for another CPU/OS?)"
	ok "using $BINARY ($("$BINARY" --version))"
else
	[ -f "$REPO/Cargo.toml" ] || die "run this from a checkout of the cookie-web repository (or pass --binary)"
	NEED=()
	for pkg in build-essential pkg-config curl ca-certificates; do dpkg -s "$pkg" >/dev/null 2>&1 || NEED+=("$pkg"); done
	if [ ${#NEED[@]} -gt 0 ]; then
		$SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -qq
		$SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${NEED[@]}" >/dev/null
		ok "installed: ${NEED[*]}"
	fi
	# shellcheck disable=SC2016 # $HOME must expand in the builder's shell
	CARGO="$(as_builder bash -lc 'command -v cargo || echo "$HOME/.cargo/bin/cargo"')"
	if ! as_builder test -x "$CARGO"; then
		echo "  installing Rust (rustup) for $BUILD_USER…"
		as_builder bash -c 'curl --proto "=https" --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal' >/dev/null
		# shellcheck disable=SC2016
		CARGO="$(as_builder bash -c 'echo "$HOME/.cargo/bin/cargo"')"
	fi
	ok "$(as_builder "$CARGO" --version)"
	echo "  compiling (a few minutes; much longer on a 4-core mini PC)…"
	# low priority: compiling uses every core for several minutes
	(cd "$REPO" && as_builder nice -n 10 "$CARGO" build --release --locked --quiet) || die "the build failed"
	BINARY="$REPO/target/release/cookie-web"
	ok "built $("$BINARY" --version)"
fi
$SUDO install -m 755 "$BINARY" /usr/local/bin/cookie-web
ok "installed /usr/local/bin/cookie-web"

# --------------------------------------------------------------- 2. config
step "Configuring"
if ! id cookie >/dev/null 2>&1; then
	$SUDO useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin cookie
	ok "created system user 'cookie'"
fi
$SUDO mkdir -p "$DATA/reports"
$SUDO chown -R cookie:cookie "$DATA"

if [ ! -f "$ENV_FILE" ]; then
	if [ -f "$REPO/deploy/cookie-web.env" ]; then
		$SUDO install -m 640 "$REPO/deploy/cookie-web.env" "$ENV_FILE"
	else
		printf 'COOKIE_HOST=0.0.0.0\nCOOKIE_PORT=8765\nCOOKIE_REPORTS_DIR=%s/reports\nOLLAMA_HOST=http://127.0.0.1:11434\n' "$DATA" |
			$SUDO tee "$ENV_FILE" >/dev/null
	fi
	set_env COOKIE_API_KEY "" # filled in below
	ok "created $ENV_FILE"
else
	ok "keeping your settings in $ENV_FILE"
fi
[ -n "$MODEL" ] && set_env COOKIE_MODEL "$MODEL"
[ -n "$FAST_MODEL" ] && set_env COOKIE_FAST_MODEL "$FAST_MODEL"
[ -n "$PORT" ] && set_env COOKIE_PORT "$PORT"
[ -n "$HOST" ] && set_env COOKIE_HOST "$HOST"
[ -n "$OLLAMA_URL" ] && set_env OLLAMA_HOST "$OLLAMA_URL"
[ -n "$THREADS" ] && set_env COOKIE_NUM_THREAD "$THREADS"
[ -z "$(get_env COOKIE_MODEL)" ] && set_env COOKIE_MODEL "qwen3:4b-instruct"
set_env COOKIE_REPORTS_DIR "$DATA/reports"

if [ "$NO_KEY" -eq 1 ]; then
	$SUDO sed -i -E "s|^COOKIE_API_KEY=.*|# COOKIE_API_KEY=|" "$ENV_FILE"
	API_KEY=""
elif [ -n "$API_KEY" ]; then
	set_env COOKIE_API_KEY "$API_KEY"
else
	API_KEY="$(get_env COOKIE_API_KEY)"
	if [ -z "$API_KEY" ] || [ "$API_KEY" = "change-me" ]; then
		API_KEY="$(head -c 32 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 32)"
		set_env COOKIE_API_KEY "$API_KEY"
		ok "generated an API key"
	fi
fi
if [ -n "$PASSWORD" ]; then
	set_env COOKIE_PASSWORD "$PASSWORD"
	ok "website password set"
fi
$SUDO chown root:cookie "$ENV_FILE"
$SUDO chmod 640 "$ENV_FILE"
PORT="$(get_env COOKIE_PORT)"
PORT="${PORT:-8765}"
HOST="$(get_env COOKIE_HOST)"
ok "model $(get_env COOKIE_MODEL) · listening on ${HOST:-0.0.0.0}:$PORT"

OLLAMA="$(get_env OLLAMA_HOST)"
if ! curl -fsS --max-time 3 "${OLLAMA:-http://127.0.0.1:11434}/api/tags" >/dev/null 2>&1; then
	warn "Ollama is not answering at ${OLLAMA:-http://127.0.0.1:11434} — run scripts/install-ollama.sh first"
fi

# ---------------------------------------------------------------- 3. start
step "Starting cookie-web"
if systemd_running; then
	if [ -f "$REPO/deploy/cookie-web.service" ]; then
		$SUDO install -m 644 "$REPO/deploy/cookie-web.service" /etc/systemd/system/cookie-web.service
	elif [ ! -f /etc/systemd/system/cookie-web.service ]; then
		die "deploy/cookie-web.service not found — run from a checkout of the repository"
	fi
	$SUDO systemctl daemon-reload
	$SUDO systemctl enable --quiet cookie-web
	$SUDO systemctl restart cookie-web
	MANAGED="systemd (starts on boot, restarts if it stops)"
	LOG=""
else
	warn "systemd is not running here, so cookie-web won't start on boot"
	pkill -x cookie-web 2>/dev/null || true
	LOG=/tmp/cookie-web.log
	(
		set -a
		# shellcheck disable=SC1090
		. <($SUDO cat "$ENV_FILE")
		set +a
		cd "$DATA"
		# fully detached, so nothing holds this script's output open
		nohup /usr/local/bin/cookie-web serve >"$LOG" 2>&1 </dev/null &
	)
	MANAGED="a background process (log: $LOG)"
fi

for _ in $(seq 1 60); do
	curl -fsS --max-time 2 "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1 && break
	sleep 1
done
HEALTH="$(curl -fsS --max-time 10 "http://127.0.0.1:$PORT/api/health" 2>/dev/null)" ||
	die "cookie-web did not start — check: journalctl -u cookie-web -n 50"
case "$HEALTH" in
*'"status":"ok"'*) ok "running under $MANAGED" ;;
*) warn "running under $MANAGED, but degraded: $(printf '%s' "$HEALTH" | sed -n 's/.*"error": *"\([^"]*\)".*/\1/p')" ;;
esac

if [ "$OPEN_FIREWALL" -eq 1 ] && command -v ufw >/dev/null && $SUDO ufw status | grep -q "Status: active"; then
	$SUDO ufw allow "$PORT/tcp" >/dev/null
	ok "opened port $PORT in ufw"
fi

step "Done"
if [ "${HOST:-0.0.0.0}" = "127.0.0.1" ]; then
	echo "  GUI:      http://127.0.0.1:$PORT   (this machine only — rerun without --local to open it up)"
else
	for ip in $(hostname -I 2>/dev/null); do
		case "$ip" in *:*) ;; *) echo "  GUI:      http://$ip:$PORT" ;; esac
	done
	echo "            (on a VPS: http://<public IP>:$PORT — allow TCP $PORT in the firewall / security group;"
	echo "             --open-firewall does it for ufw)"
fi
if [ -n "$API_KEY" ]; then
	if [ -n "$(get_env COOKIE_PASSWORD)" ]; then
		echo "  Website:  sign in with the password you set (COOKIE_PASSWORD)"
	else
		echo "  Website:  sign in with the API key below as the password"
	fi
	echo "  API key:  $API_KEY   (programs: Authorization: Bearer <key>)"
	echo "            stored in $ENV_FILE"
fi
echo "  Check:    sudo bash -c 'set -a; . $ENV_FILE; cookie-web doctor'"
if [ -n "${LOG:-}" ]; then echo "  Logs:     $LOG"; else echo "  Logs:     journalctl -u cookie-web -f"; fi
