# cookie-web console

A test client for the cookie-web API, to run on **another machine** (your laptop) against the server.
It is one self-contained HTML file: no install, no build, no internet needed besides the server.

```bash
# just open it
xdg-open client/console.html          # Linux  (macOS: open, Windows: start)
# or serve it, e.g. to open it from a phone on the same network
python3 -m http.server 8080 -d client # then http://<this machine>:8080/console.html
```

At the top, fill in:

- **Server** — the VPS: `203.0.113.5` or `cookie.example.com` (behind nginx, the default setup), or
  `203.0.113.5:8765` when cookie-web is served directly on its port. `https://` works too.
- **API key** — `COOKIE_API_KEY` from `/etc/cookie-web.env` on the server. Tick *remember* to keep it in this
  browser (otherwise it's forgotten when the tab closes).
- **Watchdog** — leave empty behind nginx (it is at `/watch/` on the same server). Without nginx the watchdog only
  listens on the server itself; it isn't reachable from here.

What it shows:

- **Live status** (left) — from `GET /api/status/stream`: the server, model, observed latency, the **queue**
  (running, paused and queued jobs in order, each queued job's place in line) with Pause / Resume / Cancel /
  Events / Details / Report buttons, recently finished jobs, search engines. Next to it the **watchdog's** view
  from `GET /watch/status`, polled directly — so when cookie-web is down the console still says *offline* and why.
- **API explorer** (middle) — every endpoint, with a form for its path, query, header and body parameters.
  Streams (`/api/research/stream`, `/api/jobs/{id}/events`, `/api/status/stream`) show their events live, with the
  answer assembling from the `token` events; Stop closes the stream (the job keeps running on the server).
  Job and report ids you've seen are suggested in the id fields.
- **Calls** (right) — every request the console made, newest first: the exact request (method, URL, headers,
  body), the response (status, headers, body, timing, or the stream's events), and **curl** to copy it.
  The API key is masked unless you tick *show the API key*. Background polling is hidden unless you tick it.

It needs CORS to be allowed on the server, which it is by default (`COOKIE_CORS_ORIGINS=*`). If you have set
`COOKIE_CORS_ORIGINS` to a list, add the console's origin (or `null` when you open the file directly).
A console opened over `https://` can't call an `http://` server (the browser blocks mixed content); open the file
directly or over http instead.
