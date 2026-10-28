#!/usr/bin/env bash
# One command for a fresh Ubuntu box: Ollama + models, then cookie-web as a service.
#
#   ./scripts/setup-ubuntu.sh                          # qwen3:4b-instruct, serve on the network with an API key
#   ./scripts/setup-ubuntu.sh --model qwen3:8b --local
#
# Options: --model NAME, --fast-model NAME (both scripts); every other option goes to
# install-cookie-web.sh (--port, --local, --api-key, --no-key, --binary, --open-firewall).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

OLLAMA_ARGS=()
COOKIE_ARGS=()
while [ $# -gt 0 ]; do
	case "$1" in
	--model | --fast-model) OLLAMA_ARGS+=("$1" "${2:?$1 needs a value}"); COOKIE_ARGS+=("$1" "$2"); shift 2 ;;
	--port | --api-key | --binary | --ollama | --threads) COOKIE_ARGS+=("$1" "${2:?$1 needs a value}"); shift 2 ;;
	-h | --help) sed -n '2,8p' "$0"; exit 0 ;;
	*) COOKIE_ARGS+=("$1"); shift ;;
	esac
done

"$HERE/install-ollama.sh" "${OLLAMA_ARGS[@]}"
"$HERE/install-cookie-web.sh" "${COOKIE_ARGS[@]}"
