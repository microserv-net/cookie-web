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
  };

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

  function logLine(cls, html) {
    const li = document.createElement("li");
    li.className = cls;
    li.innerHTML = html;
    els.log.appendChild(li);
    els.log.scrollTop = els.log.scrollHeight;
    els.activityCount.textContent = `· ${els.log.children.length} steps`;
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
    els.stats.textContent = `${r.sources.length} sources · ${s.pages_read} pages read · ${s.tool_calls} tool calls · ${(s.duration_ms / 1000).toFixed(1)}s · ${r.model}`;
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
    resetRun(query);
    setBusy(true);
    const depth = document.querySelector('input[name="depth"]:checked').value;
    const body = { query, depth, save: els.save.checked };
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
          current = { markdown: r.markdown, id: r.id };
          renderMarkdown(r.markdown);
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
      current = { markdown: r.markdown, id: r.id };
      renderMarkdown(r.markdown);
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
      const h = await (await fetch("/api/health")).json();
      els.status.className = "pill " + (h.status === "ok" ? "ok" : "degraded");
      const m = h.model || {};
      els.statusText.textContent = h.status === "ok"
        ? `${m.model}${m.supports_tools ? "" : " (no native tools)"} · ${searchSummary(h)}`
        : (m.error || "degraded");
      els.models.innerHTML = (m.installed || []).map((x) => `<option value="${esc(x)}">`).join("");
    } catch {
      els.status.className = "pill down";
      els.statusText.textContent = "server unreachable";
    }
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

  checkHealth();
  loadHistory();
  setInterval(checkHealth, 30000);
})();
