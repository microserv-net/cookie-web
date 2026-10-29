// cookie-web GUI. Talks to the same public API documented in docs/API.md.
(() => {
  const $ = (s) => document.querySelector(s);
  const store = {
    get(k, d) { try { const v = localStorage.getItem("cw." + k); return v === null ? d : v; } catch { return d; } },
    set(k, v) { try { localStorage.setItem("cw." + k, v); } catch {} },
  };

  const els = {
    q: $("#q"), go: $("#go"), form: $("#composer"), welcome: $("#welcome"), run: $("#run"),
    runQuery: $("#run-query"), triage: $("#triage"), activity: $("#activity"), activityTitle: $("#activity-title"),
    activityCount: $("#activity-count"), log: $("#log"), answer: $("#answer"), toolbar: $("#toolbar"),
    stats: $("#stats"), raw: $("#raw-link"), history: $("#history"), status: $("#status"), statusText: $("#status-text"),
    settings: $("#settings"), apiKey: $("#api-key"), model: $("#model"), save: $("#save"), models: $("#models"),
    notice: $("#notice"), noticeTitle: $("#notice-title"), noticeList: $("#notice-list"),
    follow: $("#follow"), followWrap: $("#follow-wrap"),
    statusJobs: $("#status-jobs"), statusPanel: $("#status-panel"), logout: $("#logout-btn"),
    stJobs: $("#st-jobs"), stServer: $("#st-server"), stSearch: $("#st-search"), stUpdated: $("#st-updated"),
  };
  const PLACEHOLDER = els.q.placeholder;

  let current = null; // { markdown, id }
  let controller = null;
  let running = null; // { jobId } of the job this page is following
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  // ---------------------------------------------------------------- helpers
  function headers(extra = {}) {
    const h = { ...extra };
    const key = store.get("apiKey", "");
    if (key) h["Authorization"] = "Bearer " + key;
    return h;
  }

  async function api(path, opts = {}) {
    const r = await fetch(path, { ...opts, headers: headers(opts.headers || {}) });
    if (r.status === 401 && !store.get("apiKey", "")) {
      // signed out (session expired, password changed): back to the sign-in page
      location.href = "/login?next=" + encodeURIComponent(location.pathname);
      throw new Error("not signed in");
    }
    if (!r.ok) {
      let msg = r.statusText;
      try { msg = (await r.json()).error || msg; } catch {}
      throw new Error(msg);
    }
    return r;
  }

  function esc(s) {
    return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  }

  function renderMarkdown(md, streaming = false) {
    let html;
    if (window.marked && window.DOMPurify) {
      html = DOMPurify.sanitize(marked.parse(md, { gfm: true, breaks: false }), { ADD_ATTR: ["target"] });
    } else {
      html = "<pre>" + esc(md) + "</pre>";
    }
    els.answer.innerHTML = html + (streaming ? '<span class="caret"></span>' : "");
    els.answer.querySelectorAll("a").forEach((a) => {
      if (/^\[\d+\]$/.test(a.textContent.trim())) a.classList.add("cite");
      if (a.href.startsWith("http")) { a.target = "_blank"; a.rel = "noopener"; }
    });
  }

  // ------------------------------------------------------------ follow-ups
  // A saved answer can be followed up: the next question is answered from its sources first.
  function setFollowable(report) {
    const ok = !!(report && report.saved);
    els.followWrap.hidden = !ok;
    if (ok) els.follow.checked = true;
    updatePlaceholder();
  }

  function updatePlaceholder() {
    els.q.placeholder = !els.followWrap.hidden && els.follow.checked
      ? "Ask a follow-up… (@3 asks about source 3)"
      : PLACEHOLDER;
  }

  function sourceUrl(id, n, format) {
    const key = store.get("apiKey", "");
    return `/api/reports/${encodeURIComponent(id)}/sources/${n}?format=${format}` +
      (key ? `&api_key=${encodeURIComponent(key)}` : "");
  }

  // Next to each entry of the Sources list: "text" opens the source's full content, "ask" starts a
  // follow-up about that source.
  function decorateSources(id) {
    const heading = [...els.answer.querySelectorAll("h2")].find((h) => h.textContent.trim() === "Sources");
    if (!heading) return;
    for (let el = heading.nextElementSibling; el && el.tagName !== "H2"; el = el.nextElementSibling) {
      if (el.tagName !== "OL") continue;
      const start = parseInt(el.getAttribute("start") || "1", 10);
      [...el.children].forEach((li, i) => {
        const n = start + i;
        const tools = document.createElement("span");
        tools.className = "src-tools";
        tools.innerHTML = `<a href="${sourceUrl(id, n, "text")}" target="_blank" rel="noopener" title="Full text of source ${n}">text</a>` +
          `<button type="button" title="Ask about source ${n}">ask</button>`;
        tools.querySelector("button").addEventListener("click", () => {
          els.followWrap.hidden = false;
          els.follow.checked = true;
          updatePlaceholder();
          if (!/@\d/.test(els.q.value)) els.q.value = `@${n} ` + els.q.value;
          els.q.focus();
        });
        li.appendChild(tools);
      });
    }
  }

  const clock = (ms) => {
    const s = Math.max(0, Math.round(ms / 1000));
    return s < 3600 ? `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}` : `${Math.floor(s / 3600)}h${String(Math.floor(s / 60) % 60).padStart(2, "0")}`;
  };
  let runStarted = null;
  function updateActivityCount() {
    const steps = els.log.children.length;
    const t = runStarted ? ` · ${clock(Date.now() - runStarted)}` : "";
    els.activityCount.textContent = `· ${steps} step${steps === 1 ? "" : "s"}${t}`;
  }

  function logLine(cls, html) {
    const li = document.createElement("li");
    li.className = cls;
    li.innerHTML = html;
    els.log.appendChild(li);
    els.log.scrollTop = els.log.scrollHeight;
    updateActivityCount();
    return li;
  }

  function argText(args) {
    if (!args || typeof args !== "object") return "";
    const v = args.query || args.url || args.topic || "";
    return v ? `<code>${esc(String(v).slice(0, 90))}</code>` : "";
  }

  function showTriage(t) {
    els.triage.innerHTML = `
      <span class="chip depth-${t.depth}">${esc(t.depth)}</span>
      <span class="chip">${esc(t.intent)}</span>
      <span class="chip" title="decided by ${esc(t.decided_by)}">${esc(t.reason)}</span>`;
  }

  function showStats(r) {
    const s = r.stats || {};
    const tokens = (s.prompt_tokens || 0) + (s.output_tokens || 0);
    const model = tokens
      ? ` · ${(tokens / 1000).toFixed(1)}k tokens, ${Math.round((s.model_ms || 0) / 1000)}s in the model`
      : "";
    els.stats.textContent = `${r.sources.length} sources · ${s.pages_read} pages read · ${s.tool_calls} tool calls · ${(s.duration_ms / 1000).toFixed(1)}s${model} · ${r.model}`;
  }

  function setBusy(b) {
    els.go.textContent = b ? "Stop" : "Search";
    els.go.classList.toggle("stop", b);
    els.activity.classList.toggle("running", b);
  }

  // Cancel the job this page is following (frees the server's slot for the next request).
  function cancelRunning() {
    const job = running && running.jobId;
    running = null;
    if (controller) controller.abort();
    if (job) fetch(`/api/jobs/${encodeURIComponent(job)}`, { method: "DELETE", headers: headers() }).catch(() => {});
  }

  function showNotice(warnings, degraded) {
    if (!warnings || !warnings.length) { els.notice.hidden = true; return; }
    els.notice.hidden = false;
    els.notice.classList.toggle("degraded", !!degraded);
    els.noticeTitle.textContent = degraded
      ? "The model could not write this answer — showing what the research found"
      : `${warnings.length} note${warnings.length > 1 ? "s" : ""} about this answer`;
    els.noticeList.innerHTML = warnings.map((w) => `<li>${esc(w)}</li>`).join("");
    els.notice.open = !!degraded;
  }

  function resetRun(query) {
    els.welcome.hidden = true;
    els.run.hidden = false;
    els.runQuery.textContent = query;
    els.triage.innerHTML = "";
    els.log.innerHTML = "";
    els.answer.innerHTML = "";
    els.toolbar.hidden = true;
    els.notice.hidden = true;
    els.activity.open = true;
    els.activity.hidden = false;
    els.activityTitle.textContent = "Working";
    els.activityCount.textContent = "";
    current = null;
  }

  // ------------------------------------------------------------- SSE reader
  async function* sse(response) {
    const reader = response.body.getReader();
    const dec = new TextDecoder();
    let buf = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += dec.decode(value, { stream: true });
      let i;
      while ((i = buf.indexOf("\n\n")) >= 0) {
        const chunk = buf.slice(0, i);
        buf = buf.slice(i + 2);
        let event = "message", data = "", id = null;
        for (const line of chunk.split("\n")) {
          if (line.startsWith("event:")) event = line.slice(6).trim();
          else if (line.startsWith("data:")) data += line.slice(5).trimStart();
          else if (line.startsWith("id:")) id = parseInt(line.slice(3).trim(), 10);
        }
        if (data) {
          try { yield { event, id, data: JSON.parse(data) }; } catch {}
        }
      }
    }
  }

  // ----------------------------------------------------------------- run
  async function research(query) {
    query = query.trim();
    if (!query) return;
    cancelRunning();
    const ctl = (controller = new AbortController());
    const me = (running = { jobId: null });
    const parent = current && current.saved && !els.followWrap.hidden && els.follow.checked ? current.id : null;
    resetRun(parent ? `↳ ${query}` : query);
    setBusy(true);
    runStarted = Date.now();
    const timer = setInterval(updateActivityCount, 1000);
    const depth = document.querySelector('input[name="depth"]:checked').value;
    const body = { query, depth, save: els.save.checked };
    if (parent) body.report = parent;
    // the question now shows as the heading; clear the box for the next (follow-up) question
    els.q.value = "";
    els.q.style.height = "auto";
    const model = store.get("model", "");
    if (model) body.model = model;

    let md = "";
    let pending = false;
    let lastId = 0;
    let finished = false;
    const tools = new Map();
    const flush = () => { pending = false; if (!finished) renderMarkdown(md, true); };

    const handle = (event, data) => {
      switch (event) {
        case "started": me.jobId = data.job_id; break;
        case "triage": showTriage(data); break;
        case "status": logLine("status", esc(data.message)); break;
        case "warning": logLine("warn", "⚠ " + esc(data.message)); break;
        case "tool_call": tools.set(data.call_id, logLine("tool", `<b>${esc(data.tool)}</b> ${argText(data.args)}`)); break;
        case "tool_result": {
          const li = tools.get(data.call_id);
          if (li) {
            li.classList.add(data.ok ? "ok" : "fail");
            li.innerHTML += ` <span class="muted">— ${esc(data.summary)}</span>`;
          }
          break;
        }
        case "reset":
          md = "";
          renderMarkdown("", true);
          logLine("warn", "↺ " + esc(data.reason));
          break;
        case "token":
          md += data.text;
          if (!pending) { pending = true; requestAnimationFrame(flush); }
          break;
        case "done": {
          finished = true;
          const r = data.result;
          current = { markdown: r.markdown, id: r.id, saved: !!r.report_path };
          renderMarkdown(r.markdown);
          if (current.saved) decorateSources(r.id);
          setFollowable(current);
          showStats(r);
          showNotice(r.warnings, r.degraded);
          els.toolbar.hidden = false;
          els.raw.hidden = !r.report_path;
          els.raw.href = `/api/reports/${encodeURIComponent(r.id)}/markdown`;
          els.activityTitle.textContent = r.degraded ? "Done (degraded)" : "Done";
          els.activity.open = false;
          loadHistory();
          break;
        }
        case "error":
          finished = true;
          if (data.message === "cancelled") {
            els.activityTitle.textContent = "Stopped";
            if (!md) els.answer.innerHTML = '<p class="muted">Stopped.</p>';
            break;
          }
          logLine("error", "Error: " + esc(data.message));
          els.activityTitle.textContent = "Failed";
          if (!md) els.answer.innerHTML = `<p class="muted">Something went wrong: ${esc(data.message)}</p>`;
          break;
      }
    };

    const consume = async (resp) => {
      for await (const { event, id, data } of sse(resp)) {
        if (id) lastId = id;
        handle(event, data);
      }
    };

    try {
      const resp = await api("/api/research/stream", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
        signal: ctl.signal,
      });
      me.jobId = resp.headers.get("x-cookie-job") || me.jobId;
      try { await consume(resp); } catch (e) { if (e.name === "AbortError") throw e; }
      // The connection dropped before the job finished (Wi-Fi blip, proxy timeout, laptop sleep):
      // the job is still running on the server, so pick the stream up where it left off.
      for (let attempt = 0; !finished && me.jobId && attempt < 6; attempt++) {
        logLine("status", "Connection lost — reconnecting…");
        await sleep(Math.min(1000 * 2 ** attempt, 8000));
        if (ctl.signal.aborted) return;
        try {
          const r = await api(`/api/jobs/${encodeURIComponent(me.jobId)}/events?from=${lastId}`, { signal: ctl.signal });
          await consume(r);
        } catch (e) {
          if (e.name === "AbortError") throw e;
          if (/not found/i.test(e.message)) break; // the server restarted and forgot the job
        }
      }
      if (!finished) handle("error", { message: "lost the connection to the server" });
    } catch (e) {
      if (e.name !== "AbortError") {
        logLine("error", "Error: " + esc(e.message));
        els.activityTitle.textContent = "Failed";
      }
    } finally {
      clearInterval(timer);
      updateActivityCount();
      runStarted = null;
      if (running === me) running = null;
      if (controller === ctl) setBusy(false);
    }
  }

  async function openReport(id) {
    try {
      const r = await (await api(`/api/reports/${encodeURIComponent(id)}`)).json();
      resetRun(r.query);
      showTriage(r.triage);
      els.activity.hidden = true;
      current = { markdown: r.markdown, id: r.id, saved: true };
      renderMarkdown(r.markdown);
      decorateSources(r.id);
      setFollowable(current);
      showStats(r);
      showNotice(r.warnings, r.degraded);
      els.toolbar.hidden = false;
      els.raw.hidden = false;
      els.raw.href = `/api/reports/${encodeURIComponent(r.id)}/markdown`;
      document.querySelectorAll("#history a").forEach((a) => a.classList.toggle("active", a.dataset.id === id));
      document.body.classList.remove("menu-open");
    } catch (e) {
      alert("Could not open report: " + e.message);
    }
  }

  async function loadHistory() {
    try {
      const { reports } = await (await api("/api/reports?limit=100")).json();
      if (!reports.length) { els.history.innerHTML = '<li class="muted">No reports yet</li>'; return; }
      els.history.innerHTML = reports.map((r) => `
        <li><a href="#" data-id="${esc(r.id)}">${esc(r.query)}
          <small>${esc(r.depth)} · ${new Date(r.created_at).toLocaleString()}</small></a></li>`).join("");
    } catch {
      els.history.innerHTML = '<li class="muted">History unavailable</li>';
    }
  }

  function searchSummary(h) {
    const list = h.search_health || (h.search_backends || []).map((name) => ({ name }));
    return list.map((b) => (b.cooldown_secs ? `${b.name} (resting)` : b.name)).join(", ");
  }

  async function checkHealth() {
    try {
      const h = await (await fetch("/api/health", { headers: headers() })).json();
      els.status.className = "pill " + (h.status === "ok" ? "ok" : "degraded");
      const m = h.model || {};
      els.statusText.textContent = h.status === "ok"
        ? `${m.model}${m.supports_tools ? "" : " (no native tools)"} · ${searchSummary(h)}`
        : (m.error || "degraded");
      els.models.innerHTML = (m.installed || []).map((x) => `<option value="${esc(x)}">`).join("");
      els.logout.hidden = !h.auth_required;
      const busy = (h.jobs_running || 0) + (h.jobs_queued || 0);
      els.statusJobs.hidden = !busy;
      els.statusJobs.textContent = h.jobs_queued ? `${h.jobs_running} running · ${h.jobs_queued} queued` : `${h.jobs_running} running`;
      return h;
    } catch {
      els.status.className = "pill down";
      els.statusText.textContent = "server unreachable";
      return null;
    }
  }

  // ------------------------------------------------------------ status view
  let statusTimer = null;
  const gb = (b) => (b ? `${(b / 1e9).toFixed(2)} GB` : "—");
  const ago = (iso) => {
    const s = Math.round((Date.now() - new Date(iso)) / 1000);
    return s < 60 ? `${s}s ago` : s < 3600 ? `${Math.floor(s / 60)}m ago` : new Date(iso).toLocaleString();
  };

  function renderJobs(jobs) {
    if (!jobs.length) { els.stJobs.innerHTML = '<li class="muted">No jobs yet</li>'; return; }
    els.stJobs.innerHTML = jobs.slice(0, 20).map((j) => {
      const live = j.status === "running" || j.status === "queued";
      const time = j.status === "queued" ? `waiting ${clock(j.queued_ms)}`
        : j.status === "running" ? clock(j.running_ms) : `took ${clock(j.running_ms)}`;
      const detail = live ? esc(j.stage || "starting…")
        : j.status === "done" ? `${j.sources} sources · ${j.tool_calls} tool calls${j.warnings ? ` · ${j.warnings} notes` : ""}${j.degraded ? " · degraded" : ""}`
        : esc(j.error || "");
      const actions = live ? `<button type="button" data-stop="${esc(j.id)}">Stop</button>`
        : j.report_id ? `<button type="button" data-open="${esc(j.report_id)}">Open</button>` : "";
      return `<li class="st-job ${esc(j.status)}">
        <div class="st-job-top"><span class="chip st-${esc(j.status)}">${esc(j.status)}</span>
          ${j.depth ? `<span class="chip">${esc(j.depth)}</span>` : ""}
          <span class="st-time">${time}</span><span class="muted small">${ago(j.created_at)}</span>${actions}</div>
        <div class="st-q">${esc(j.query)}</div>
        <div class="muted small">${detail}</div></li>`;
    }).join("");
  }

  function renderServer(h) {
    const m = h.model || {};
    const row = (k, v) => `<dt>${k}</dt><dd>${v}</dd>`;
    const up = h.uptime_secs != null ? clock(h.uptime_secs * 1000) : "—";
    els.stServer.innerHTML = [
      row("Status", `<span class="chip st-${h.status === "ok" ? "done" : "error"}">${esc(h.status)}</span>`),
      row("Model", `${esc(m.model || "—")}${m.supports_tools ? "" : " (no native tools)"}`),
      row("Ollama", m.reachable ? "reachable" : `<span class="err">${esc(m.error || "unreachable")}</span>`),
      row("Loaded", m.loaded ? `yes · ${gb(m.memory_bytes)}${m.context_length ? ` · ${m.context_length} ctx` : ""}` : "no (loads on the next request)"),
      row("Jobs", `${h.jobs_running} running · ${h.jobs_queued} queued · ${h.max_concurrent} at a time`),
      row("Settings", `ctx ${h.num_ctx} · threads ${h.num_thread || "all"} · ${esc(h.agent_mode)} · fact check ${h.fact_check ? "on" : "off"}`),
      row("Version", `${esc(h.version)} · up ${up}`),
    ].join("");
    const engines = h.search_health || [];
    els.stSearch.innerHTML = `<tr><th>engine</th><th>state</th><th>ok</th><th>failed</th><th>last error</th></tr>` +
      engines.map((b) => `<tr><td>${esc(b.name)}</td>
        <td>${b.cooldown_secs ? `<span class="warn">resting ${clock(b.cooldown_secs * 1000)}</span>` : '<span class="okc">ready</span>'}</td>
        <td>${b.ok}</td><td>${b.failures}</td>
        <td class="muted small" title="${esc(b.last_error || "")}">${esc((b.last_error || "").slice(0, 60))}</td></tr>`).join("");
  }

  async function refreshStatus() {
    try {
      const [h, j] = await Promise.all([checkHealth(), api("/api/jobs").then((r) => r.json())]);
      if (h) renderServer(h);
      renderJobs(j.jobs || []);
      els.stUpdated.textContent = "updated " + new Date().toLocaleTimeString();
    } catch (e) {
      els.stUpdated.textContent = "update failed: " + e.message;
    }
  }

  function openStatus() {
    refreshStatus();
    clearInterval(statusTimer);
    statusTimer = setInterval(refreshStatus, 2000);
    els.statusPanel.showModal();
  }

  // ------------------------------------------------------------- wiring
  els.form.addEventListener("submit", (e) => {
    e.preventDefault();
    if (running) { // the button reads "Stop" while working
      cancelRunning();
      setBusy(false);
      els.activityTitle.textContent = "Stopped";
      return;
    }
    research(els.q.value);
  });
  els.q.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); research(els.q.value); }
  });
  els.q.addEventListener("input", () => { els.q.style.height = "auto"; els.q.style.height = els.q.scrollHeight + "px"; });
  document.querySelectorAll(".example").forEach((b) => b.addEventListener("click", () => { els.q.value = b.textContent; research(b.textContent); }));
  els.history.addEventListener("click", (e) => {
    const a = e.target.closest("a[data-id]");
    if (a) { e.preventDefault(); openReport(a.dataset.id); }
  });
  $("#new-btn").addEventListener("click", () => {
    cancelRunning();
    setBusy(false);
    current = null;
    setFollowable(null);
    els.run.hidden = true; els.welcome.hidden = false; els.q.value = ""; els.q.focus();
    document.body.classList.remove("menu-open");
  });
  $("#menu-btn").addEventListener("click", () => document.body.classList.toggle("menu-open"));
  $("#copy-btn").addEventListener("click", async () => {
    if (!current) return;
    try { await navigator.clipboard.writeText(current.markdown); $("#copy-btn").textContent = "Copied ✓"; }
    catch { $("#copy-btn").textContent = "Copy failed"; }
    setTimeout(() => ($("#copy-btn").textContent = "Copy Markdown"), 1500);
  });
  $("#dl-btn").addEventListener("click", () => {
    if (!current) return;
    const a = document.createElement("a");
    a.href = URL.createObjectURL(new Blob([current.markdown], { type: "text/markdown" }));
    a.download = (current.id || "cookie-web") + ".md";
    a.click();
    URL.revokeObjectURL(a.href);
  });
  $("#settings-btn").addEventListener("click", () => {
    els.apiKey.value = store.get("apiKey", "");
    els.model.value = store.get("model", "");
    els.settings.showModal();
  });
  els.settings.addEventListener("close", () => {
    store.set("apiKey", els.apiKey.value.trim());
    store.set("model", els.model.value.trim());
    store.set("save", els.save.checked ? "1" : "0");
    loadHistory();
  });
  els.save.checked = store.get("save", "1") === "1";
  els.follow.addEventListener("change", updatePlaceholder);
  els.status.addEventListener("click", openStatus);
  $("#status-close").addEventListener("click", () => els.statusPanel.close());
  els.statusPanel.addEventListener("close", () => clearInterval(statusTimer));
  els.stJobs.addEventListener("click", async (e) => {
    const stop = e.target.closest("[data-stop]");
    const open = e.target.closest("[data-open]");
    if (stop) {
      stop.disabled = true;
      await api(`/api/jobs/${encodeURIComponent(stop.dataset.stop)}`, { method: "DELETE" }).catch(() => {});
      refreshStatus();
    } else if (open) {
      els.statusPanel.close();
      openReport(open.dataset.open);
    }
  });

  checkHealth();
  loadHistory();
  setInterval(checkHealth, 10000);
})();
