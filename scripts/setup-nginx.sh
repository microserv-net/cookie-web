#!/usr/bin/env bash
# Put nginx in front of cookie-web (and its watchdog), optionally with HTTPS.
#
#   ./scripts/setup-nginx.sh                                     # http://<server IP>/
#   ./scripts/setup-nginx.sh --domain cookie.example.com --email you@example.com   # + HTTPS (Let's Encrypt)
#
# cookie-web and cookie-watch keep listening on 127.0.0.1; nginx serves port 80 (and 443).
# Live progress streams through unbuffered; when cookie-web is down, pages go to the watchdog's
# status page (/watch/) and API calls get a JSON 503 instead of "502 Bad Gateway".
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE=/etc/cookie-web.env
DOMAIN=""
EMAIL=""
PORT=""
WATCH_PORT=""

usage() {
	cat <<'USAGE'
Usage: setup-nginx.sh [options]

  --domain NAME    the site's domain (its DNS A/AAAA record must point here); enables HTTPS
  --email ADDR     for Let's Encrypt expiry notices (with --domain)
  --port N         cookie-web's port (default: COOKIE_PORT from /etc/cookie-web.env, else 8765)
  --watch-port N   the watchdog's port (default: COOKIE_WATCH_PORT, else 8790)
  -h, --help       this help
USAGE
}

while [ $# -gt 0 ]; do
	case "$1" in
	--domain) DOMAIN="${2:?}"; shift 2 ;;
	--email) EMAIL="${2:?}"; shift 2 ;;
	--port) PORT="${2:?}"; shift 2 ;;
	--watch-port) WATCH_PORT="${2:?}"; shift 2 ;;
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
get_env() { $SUDO sed -n -E "s/^${1}=(.*)/\1/p" "$ENV_FILE" 2>/dev/null | tail -1; }
[ -f "$REPO/deploy/nginx/cookie-web.conf" ] || die "run this from a checkout of the cookie-web repository"
if [ -n "$DOMAIN" ] && ! [[ "$DOMAIN" =~ ^[A-Za-z0-9.-]+$ ]]; then die "bad domain: $DOMAIN"; fi

PORT="${PORT:-$(get_env COOKIE_PORT)}"
PORT="${PORT:-8765}"
WATCH_PORT="${WATCH_PORT:-$(get_env COOKIE_WATCH_PORT)}"
WATCH_PORT="${WATCH_PORT:-8790}"

step "Installing nginx"
if ! command -v nginx >/dev/null; then
	$SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -qq
	$SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq nginx >/dev/null
fi
ok "$(nginx -v 2>&1)"

step "Configuring the site"
$SUDO mkdir -p /etc/nginx/snippets /etc/nginx/sites-available /etc/nginx/sites-enabled
$SUDO install -m 644 "$REPO/deploy/nginx/cookie-web-proxy.conf" /etc/nginx/snippets/cookie-web-proxy.conf
SITE=/etc/nginx/sites-available/cookie-web
# certbot adds its HTTPS lines to this file: keep them if they're there
if [ -f "$SITE" ] && $SUDO grep -q "managed by Certbot" "$SITE"; then
	warn "keeping $SITE (it has HTTPS settings from certbot); delete it and rerun to regenerate"
else
	# no IPv6 in the kernel (some VPS images): nginx refuses to start with an IPv6 listen line
	NO_V6='/listen \[::\]/!p'
	[ -e /proc/net/if_inet6 ] && NO_V6='p'
	sed -n -e "s/__PORT__/$PORT/g" -e "s/__WATCH_PORT__/$WATCH_PORT/g" -e "s/__SERVER_NAME__/${DOMAIN:-_}/g" \
		-e "$NO_V6" "$REPO/deploy/nginx/cookie-web.conf" | $SUDO tee "$SITE" >/dev/null
	ok "wrote $SITE (cookie-web on 127.0.0.1:$PORT, watchdog on 127.0.0.1:$WATCH_PORT)"
fi
$SUDO ln -sf "$SITE" /etc/nginx/sites-enabled/cookie-web
# the stock "Welcome to nginx" site would compete for port 80
if [ -L /etc/nginx/sites-enabled/default ]; then
	$SUDO rm /etc/nginx/sites-enabled/default
	ok "disabled nginx's default welcome site"
fi
$SUDO nginx -t 2>&1 | sed 's/^/  /' || die "nginx rejected the configuration (see above)"
systemd_running() {
	command -v systemctl >/dev/null || return 1
	case "$(systemctl is-system-running 2>/dev/null || true)" in running | degraded | starting) return 0 ;; *) return 1 ;; esac
}
if systemd_running; then
	$SUDO systemctl enable --quiet nginx
	$SUDO systemctl reload nginx 2>/dev/null || $SUDO systemctl restart nginx
else
	$SUDO nginx -s reload 2>/dev/null || $SUDO nginx
fi
ok "nginx is serving cookie-web on port 80"

if [ -n "$DOMAIN" ]; then
	step "HTTPS for $DOMAIN (Let's Encrypt)"
	if ! command -v certbot >/dev/null; then
		$SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq certbot python3-certbot-nginx >/dev/null
	fi
	if [ -n "$EMAIL" ]; then CONTACT=(-m "$EMAIL"); else CONTACT=(--register-unsafely-without-email); fi
	if $SUDO certbot --nginx -d "$DOMAIN" --non-interactive --agree-tos "${CONTACT[@]}" --redirect; then
		ok "HTTPS on; http:// redirects to https://; certbot renews it automatically"
	else
		warn "certbot failed — is $DOMAIN's DNS pointing at this server, and is port 80 open?"
		warn "the site still works over http://; rerun with --domain once DNS is right"
	fi
fi

step "Checking"
sleep 1
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1/watch/status || true)"
case "$code" in
200) ok "the watchdog says cookie-web is up (http://127.0.0.1/watch/status)" ;;
503) warn "the watchdog answers but cookie-web isn't up yet (see /watch/)" ;;
*) warn "/watch/status answered $code — is cookie-watch running? (systemctl status cookie-watch)" ;;
esac
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1/api/manifest || true)"
if [ "$code" = 200 ]; then ok "cookie-web answers through nginx"; else warn "cookie-web through nginx: HTTP $code"; fi

echo
if [ -n "$DOMAIN" ]; then
	echo "  Open:     https://$DOMAIN/"
	echo "  Status:   https://$DOMAIN/watch/     (JSON: /watch/status)"
else
	for ip in $(hostname -I 2>/dev/null); do
		case "$ip" in *:*) ;; *) echo "  Open:     http://$ip/        Status: http://$ip/watch/" ;; esac
	done
	warn "plain http: the password travels unencrypted. With a domain, rerun with --domain for HTTPS."
fi
