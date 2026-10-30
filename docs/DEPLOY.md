# Deploying on a VPS (Ubuntu)

One command sets up everything on a fresh Ubuntu server:

```bash
git clone https://github.com/microserv-net/cookie-web && cd cookie-web
./scripts/setup-ubuntu.sh --password 'a long passphrase'
# with a domain whose DNS points at the server, add HTTPS:
./scripts/setup-ubuntu.sh --password '…' --domain cookie.example.com --email you@example.com
```

What you end up with:

```
 internet ──▶ ufw (22, 80, 443 only)
                 │
                 ▼
              nginx :80/:443 ─────────────┬──────────────────────────────┐
                 │  /  /api/…             │  /watch/…                    │ cookie-web down?
                 ▼                        ▼                              │ pages → /watch/
         cookie-web 127.0.0.1:8765   cookie-watch 127.0.0.1:8790 ◀───────┘ API  → 503 JSON
                 │   ── heartbeat every 5 s ──▶  │
                 │   ◀── probes /api/health ──── │  + checks /proc/<pid>
                 ▼
          Ollama 127.0.0.1:11434 (main + fast model)
```

Every script is safe to run again (it upgrades and keeps your settings), and each one also runs on its own:

| step | script | what it does |
|---|---|---|
| 1 | `scripts/install-deps.sh` | `apt update` + `build-essential pkg-config libssl-dev curl git`, then Rust via rustup for your user (not root) |
| 2 | `scripts/install-ollama.sh` | checks the machine, installs and tunes Ollama (both models kept loaded, low CPU priority), pulls the main and fast models |
| 3 | `scripts/install-cookie-web.sh --behind-proxy` | builds cookie-web and cookie-watch, creates the `cookie` user, `/etc/cookie-web.env` and both systemd services, starts them |
| 4 | `scripts/setup-nginx.sh [--domain D --email E]` | nginx in front: streaming passed through unbuffered, long timeouts for slow answers, a login rate limit, the offline fallback; HTTPS via Let's Encrypt with `--domain` |
| 5 | `scripts/setup-firewall.sh` | ufw: SSH (every port sshd listens on, plus the one you're connected through — allowed **before** the firewall goes on, so you can't lock yourself out), 80 and 443; everything else closed |

`setup-ubuntu.sh` options: `--model`, `--fast-model`, `--password`, `--api-key`, `--threads`, `--domain`, `--email`,
`--ssh-port`, `--no-nginx` (serve cookie-web directly on 0.0.0.0:8765 and open that port instead), `--no-firewall`,
`--skip-deps`, plus any `install-cookie-web.sh` option (`--port`, `--binary`, `--watch-port`, …). `--help` on any
script lists its options.

> On a cloud VPS the provider usually has its own firewall / security group as well: allow TCP 80 and 443 there.

## Is it up? Three layers

1. **systemd restarts it.** `cookie-web.service` has `Restart=always`, and it is `Type=notify` with
   `WatchdogSec=60`: cookie-web tells systemd when it is ready, then pings systemd every 20 s — but only while its own
   HTTP server answers a self-check. A crash *or a hang* gets it restarted.
2. **The watchdog knows.** `cookie-watch.service` is a separate small process (~3 MB) that isn't tied to
   cookie-web's service, so it keeps running when cookie-web is down. cookie-web registers with it (pid, version,
   port) and sends a heartbeat every 5 s; the watchdog independently checks that the pid still exists and is still
   cookie-web, and probes `/api/health`. It reports `online`, `degraded`, `unresponsive`, `offline` (with the reason:
   exited/crashed, clean shutdown, not answering) and keeps a log of the last 50 changes.
3. **You see it.**
   - `https://<server>/watch/` — status page (details once you're signed in to cookie-web);
   - `https://<server>/watch/status` — JSON, `200` when the API is up and `503` when not: point any uptime
     monitor at it;
   - the status view in the cookie-web website (click the status pill) shows the watchdog's view too;
   - when cookie-web is down, nginx sends browsers to the status page instead of "502 Bad Gateway", and it returns
     you to the app once it's back.

Heartbeats (`/watch/beat`, `/watch/bye`) are only accepted from the machine itself, never through nginx.

## Day to day

```bash
systemctl status cookie-web cookie-watch nginx ollama
journalctl -u cookie-web -f                 # cookie-web's log
journalctl -u cookie-watch -n 50            # every status change, with the reason
curl -s localhost:8790/watch/status         # on the box, without nginx
sudo ufw status verbose
```

Change settings in `/etc/cookie-web.env`, then `sudo systemctl restart cookie-web`. Upgrade: `git pull` and run
`./scripts/setup-ubuntu.sh --skip-deps` again (or just `./scripts/install-cookie-web.sh --behind-proxy`).

## Watchdog settings

Read from `/etc/cookie-web.env` like everything else:

| variable | default | |
|---|---|---|
| `COOKIE_WATCH_URL` | — (the installer sets `http://127.0.0.1:8790`) | where cookie-web sends its heartbeat; unset = no heartbeat |
| `COOKIE_WATCH_HOST` / `COOKIE_WATCH_PORT` | `127.0.0.1` / `8790` | where the watchdog listens |
| `COOKIE_WATCH_TARGET` | `http://127.0.0.1:$COOKIE_PORT` | the cookie-web it watches |
| `COOKIE_WATCH_INTERVAL_SECS` | `5` | time between checks |

The watchdog uses the same `COOKIE_API_KEY` / `COOKIE_PASSWORD` to decide who sees the details, and the API key for
its own `/api/health` probe.

## Testing it from your own machine

`client/console.html` (see [client/README.md](../client/README.md)) is a test console for the API: open it on your
laptop, enter the server address and the API key, and it shows the live status and queue, lets you call every
endpoint, and logs each request and response.
