#!/usr/bin/env bash
# One command for a fresh Ubuntu box: everything cookie-web needs, set up and running.
#
#   ./scripts/setup-ubuntu.sh
#   ./scripts/setup-ubuntu.sh --password 'a long passphrase' --domain cookie.example.com --email you@example.com
#
# Steps:
#   1. build tools: apt (build-essential, pkg-config, libssl-dev) + Rust     scripts/install-deps.sh
#   2. Ollama + models (main + fast), tuned for a small CPU box              scripts/install-ollama.sh
#   3. cookie-web + its watchdog (cookie-watch) as systemd services          scripts/install-cookie-web.sh
#   4. nginx in front (HTTPS with --domain)                                  scripts/setup-nginx.sh
#   5. firewall: SSH + 80/443 in, everything else closed                     scripts/setup-firewall.sh
#
# Options:
#   --model NAME, --fast-model NAME     models (default qwen3:4b-instruct + qwen3:1.7b)
#   --password PW, --api-key KEY        website password / API key (default: one generated key for both)
#   --threads N                         CPU threads the model may use (default: all)
#   --domain NAME, --email ADDR         HTTPS with Let's Encrypt (the domain's DNS must point here)
#   --no-nginx                          serve cookie-web directly on its port (0.0.0.0) instead
#   --no-firewall                       leave the firewall alone
#   --ssh-port N                        SSH port for the firewall (default: detected)
#   --skip-deps                         don't run step 1 (tools already installed)
# Anything else goes to install-cookie-web.sh (--port, --binary, --ollama, --watch-port, …).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

OLLAMA_ARGS=()
COOKIE_ARGS=()
NGINX_ARGS=()
FW_ARGS=()
NGINX=1
FIREWALL=1
DEPS=1
PORT=""
while [ $# -gt 0 ]; do
	case "$1" in
	--model | --fast-model) OLLAMA_ARGS+=("$1" "${2:?$1 needs a value}"); COOKIE_ARGS+=("$1" "$2"); shift 2 ;;
	--port) PORT="${2:?--port needs a value}"; COOKIE_ARGS+=("$1" "$2"); NGINX_ARGS+=("$1" "$2"); shift 2 ;;
	--watch-port) COOKIE_ARGS+=("$1" "${2:?}"); NGINX_ARGS+=("$1" "$2"); shift 2 ;;
	--api-key | --password | --binary | --watch-binary | --ollama | --threads) COOKIE_ARGS+=("$1" "${2:?$1 needs a value}"); shift 2 ;;
	--domain | --email) NGINX_ARGS+=("$1" "${2:?$1 needs a value}"); shift 2 ;;
	--ssh-port) FW_ARGS+=("$1" "${2:?}"); shift 2 ;;
	--no-nginx) NGINX=0; shift ;;
	--no-firewall) FIREWALL=0; shift ;;
	--skip-deps) DEPS=0; shift ;;
	-h | --help) sed -n '2,27p' "$0"; exit 0 ;;
	*) COOKIE_ARGS+=("$1"); shift ;;
	esac
done

for arg in "${COOKIE_ARGS[@]}"; do
	if [ "$arg" = "--binary" ]; then DEPS=0; fi # nothing to compile
done

if [ "$DEPS" -eq 1 ]; then "$HERE/install-deps.sh"; fi
"$HERE/install-ollama.sh" "${OLLAMA_ARGS[@]}"
if [ "$NGINX" -eq 1 ]; then
	# cookie-web on 127.0.0.1 only; nginx is the way in
	"$HERE/install-cookie-web.sh" --behind-proxy "${COOKIE_ARGS[@]}"
	"$HERE/setup-nginx.sh" "${NGINX_ARGS[@]}"
else
	"$HERE/install-cookie-web.sh" "${COOKIE_ARGS[@]}"
fi

if [ "$FIREWALL" -eq 1 ]; then
	PORT="${PORT:-$(sudo sed -n -E 's/^COOKIE_PORT=(.*)/\1/p' /etc/cookie-web.env 2>/dev/null | tail -1)}"
	PORT="${PORT:-8765}"
	if [ "$NGINX" -eq 1 ]; then
		# nginx is the way in: close cookie-web's own port if an earlier install opened it
		"$HERE/setup-firewall.sh" --close "$PORT" "${FW_ARGS[@]}"
	else
		"$HERE/setup-firewall.sh" --no-web --direct "$PORT" "${FW_ARGS[@]}"
	fi
fi
