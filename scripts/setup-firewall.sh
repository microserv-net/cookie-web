#!/usr/bin/env bash
# Firewall (ufw) for a cookie-web box: SSH and the web ports in, everything else closed.
#
#   ./scripts/setup-firewall.sh                 # SSH + 80/443 (nginx in front of cookie-web)
#   ./scripts/setup-firewall.sh --direct 8765   # no nginx: open cookie-web's own port instead
#
# SSH is allowed BEFORE the firewall is switched on (on every port sshd listens on, plus the one
# this session came in on), so running this over SSH can't lock you out.
# cookie-web, its watchdog and Ollama listen on 127.0.0.1 behind nginx and stay closed.
set -euo pipefail

WEB=1
EXTRA=()
CLOSE=()
SSH_PORTS=()

usage() {
	cat <<'USAGE'
Usage: setup-firewall.sh [options]

  --ssh-port N   SSH port to allow (default: detected from sshd; repeatable)
  --no-web       don't open 80/443
  --direct N     open port N too (cookie-web served directly, without nginx)
  --allow N      open another TCP port (repeatable)
  --close N      remove an earlier "allow N/tcp" rule (e.g. cookie-web's old direct port)
  -h, --help     this help
USAGE
}

while [ $# -gt 0 ]; do
	case "$1" in
	--ssh-port) SSH_PORTS+=("${2:?}"); shift 2 ;;
	--no-web) WEB=0; shift ;;
	--direct | --allow) EXTRA+=("${2:?}"); shift 2 ;;
	--close) CLOSE+=("${2:?}"); shift 2 ;;
	-h | --help) usage; exit 0 ;;
	*) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
	esac
done

if [ -t 1 ]; then B=$'\e[1m' G=$'\e[32m' Y=$'\e[33m' R=$'\e[31m' N=$'\e[0m'; else B='' G='' Y='' R='' N=''; fi
step() { echo; echo "${B}==> $*${N}"; }
ok() { echo "  ${G}✓${N} $*"; }
warn() { echo "  ${Y}!${N} $*"; }
die() { echo "  ${R}✗ $*${N}" >&2; exit 1; }

port_ok() { [[ "$1" =~ ^[0-9]+$ ]] && [ "$1" -ge 1 ] && [ "$1" -le 65535 ]; }
if [ "$(id -u)" -eq 0 ]; then SUDO=""; else SUDO="sudo"; sudo -v || die "sudo is required"; fi

step "Installing ufw"
if ! command -v ufw >/dev/null; then
	$SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq ufw >/dev/null
fi
ok "$(ufw version 2>/dev/null | head -1 || echo ufw)"

step "Finding the SSH port(s)"
if [ ${#SSH_PORTS[@]} -eq 0 ]; then
	# what sshd is configured to listen on…
	while read -r p; do SSH_PORTS+=("$p"); done < <($SUDO sshd -T 2>/dev/null | awk '$1 == "port" { print $2 }')
	# …and the port this very session came in on (SSH_CONNECTION="client cport server sport")
	if [ -n "${SSH_CONNECTION:-}" ]; then SSH_PORTS+=("$(echo "$SSH_CONNECTION" | awk '{ print $4 }')"); fi
	[ ${#SSH_PORTS[@]} -eq 0 ] && SSH_PORTS=(22)
fi
mapfile -t SSH_PORTS < <(printf '%s\n' "${SSH_PORTS[@]}" | sort -un)
for p in "${SSH_PORTS[@]}"; do port_ok "$p" || die "bad SSH port: $p"; done
ok "SSH on ${SSH_PORTS[*]}"

step "Setting the rules"
$SUDO ufw default deny incoming >/dev/null
$SUDO ufw default allow outgoing >/dev/null
for p in "${SSH_PORTS[@]}"; do
	$SUDO ufw allow "$p/tcp" comment 'SSH' >/dev/null
	ok "allow $p/tcp (SSH)"
done
if [ "$WEB" -eq 1 ]; then
	$SUDO ufw allow 80/tcp comment 'cookie-web (nginx, http)' >/dev/null
	$SUDO ufw allow 443/tcp comment 'cookie-web (nginx, https)' >/dev/null
	ok "allow 80/tcp, 443/tcp (nginx)"
fi
for p in "${EXTRA[@]}"; do
	port_ok "$p" || die "bad port: $p"
	$SUDO ufw allow "$p/tcp" comment 'cookie-web' >/dev/null
	ok "allow $p/tcp"
done
for p in "${CLOSE[@]}"; do
	port_ok "$p" || die "bad port: $p"
	if $SUDO ufw delete allow "$p/tcp" >/dev/null 2>&1; then ok "removed the old rule for $p/tcp"; fi
done

step "Switching the firewall on"
$SUDO ufw --force enable >/dev/null
ok "ufw is active"
$SUDO ufw status verbose | sed 's/^/  /'
echo
echo "  Everything else coming in is blocked: cookie-web (and its watchdog) and Ollama are only"
echo "  reachable through nginx or from this machine."
warn "on a cloud VPS, the provider's own firewall / security group must also allow 80 and 443"
