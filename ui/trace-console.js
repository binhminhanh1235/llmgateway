(() => {
  const LOCAL_KEY = "llmgateway.apiKey.local";
  const SESSION_KEY = "llmgateway.apiKey.session";
  const content = document.getElementById("tracesContent");
  const searchInput = document.getElementById("traceSearch");
  const refreshButton = document.getElementById("refreshTracesButton");
  if (!content || !searchInput || !refreshButton) return;

  let executions = [];
  let selectedRequestId = null;
  let selectedTrace = null;
  let loading = false;
  let detailLoading = false;

  function apiKey() {
    return localStorage.getItem(LOCAL_KEY) || sessionStorage.getItem(SESSION_KEY) || "";
  }

  function authHeaders() {
    const key = apiKey();
    return key ? { Authorization: `Bearer ${key}` } : {};
  }

  function active() {
    return document.getElementById("tracesView")?.classList.contains("active-view") === true;
  }

  async function load(force = false) {
    if (!apiKey() || loading || (!force && !active())) return;
    loading = true;
    refreshButton.disabled = true;
    refreshButton.textContent = "Refreshing…";
    try {
      const response = await fetch("/_llmgateway/executions?limit=100", { headers: authHeaders() });
      if (!response.ok) throw new Error(await errorText(response));
      executions = (await response.json()).data || [];

      if (selectedRequestId && !executions.some((item) => item.request_id === selectedRequestId)) {
        selectedRequestId = null;
        selectedTrace = null;
      }
      if (!selectedRequestId && executions.length) selectedRequestId = executions[0].request_id;
      render();
      if (selectedRequestId) await loadDetail(selectedRequestId, false);
    } catch (error) {
      content.innerHTML = `<div class="trace-error">${escapeHtml(error.message || error)}</div>`;
    } finally {
      loading = false;
      refreshButton.disabled = false;
      refreshButton.textContent = "Refresh";
    }
  }

  async function loadDetail(requestId, rerenderShell = true) {
    if (!requestId || detailLoading) return;
    selectedRequestId = requestId;
    detailLoading = true;
    if (rerenderShell) render();
    renderDetailLoading();
    try {
      const response = await fetch(`/_llmgateway/executions/${encodeURIComponent(requestId)}`, {
        headers: authHeaders(),
      });
      if (!response.ok) throw new Error(await errorText(response));
      selectedTrace = await response.json();
      render();
    } catch (error) {
      const detail = document.getElementById("traceDetail");
      if (detail) detail.innerHTML = `<div class="trace-error">${escapeHtml(error.message || error)}</div>`;
    } finally {
      detailLoading = false;
    }
  }

  function filteredExecutions() {
    const query = searchInput.value.trim().toLowerCase();
    if (!query) return executions;
    return executions.filter((item) => [
      item.request_id,
      item.requested_model,
      item.selected_route,
      item.preferred_route,
      item.status,
      item.final_error,
    ].some((value) => String(value || "").toLowerCase().includes(query)));
  }

  function render() {
    const visible = filteredExecutions();
    content.innerHTML = `
      <div class="trace-console-shell">
        <section class="trace-list-panel" aria-label="Recent execution traces">
          <div class="trace-panel-heading">
            <span>${visible.length} request${visible.length === 1 ? "" : "s"}</span>
            <span class="trace-muted">latest 100</span>
          </div>
          <div id="traceList" class="trace-list">
            ${visible.length ? visible.map(traceRow).join("") : '<div class="trace-empty">No execution traces match this filter.</div>'}
          </div>
        </section>
        <section id="traceDetail" class="trace-detail-panel" aria-label="Execution trace detail">
          ${detailHtml()}
        </section>
      </div>`;

    content.querySelectorAll("[data-trace-request]").forEach((button) => {
      button.addEventListener("click", () => loadDetail(button.dataset.traceRequest));
    });
  }

  function traceRow(item) {
    const selected = item.request_id === selectedRequestId;
    const statusClass = statusTone(item.status);
    const route = item.selected_route || "no route selected";
    const attempts = Number(item.attempt_count || 0);
    return `<button type="button" class="trace-row ${selected ? "selected" : ""}" data-trace-request="${escapeAttr(item.request_id)}">
      <div class="trace-row-top">
        <code class="trace-request-id">${escapeHtml(shortId(item.request_id))}</code>
        <span class="trace-status ${statusClass}">${escapeHtml(item.status || "unknown")}</span>
      </div>
      <div class="trace-model">${escapeHtml(item.requested_model || "unknown model")}</div>
      <div class="trace-row-meta">
        <span>${escapeHtml(route)}</span>
        <span>${attempts} attempt${attempts === 1 ? "" : "s"}</span>
      </div>
      <time class="trace-time" datetime="${escapeAttr(item.started_at || "")}">${escapeHtml(formatTime(item.started_at))}</time>
    </button>`;
  }

  function detailHtml() {
    if (!selectedRequestId) {
      return '<div class="trace-empty trace-detail-empty"><strong>No trace selected</strong><span>Send a request through llmgateway to populate the flight recorder.</span></div>';
    }
    if (!selectedTrace || selectedTrace.request_id !== selectedRequestId) {
      return '<div class="trace-empty trace-detail-empty">Loading execution timeline…</div>';
    }

    const trace = selectedTrace;
    const attempts = Array.isArray(trace.attempts) ? trace.attempts : [];
    const totalDuration = attempts.reduce((sum, attempt) => sum + Number(attempt.duration_ms || 0), 0);
    const selectedRoute = trace.selected_route || "None";
    const terminal = trace.status === "success" ? "Completed" : "Failed";
    return `
      <div class="trace-detail-header">
        <div>
          <div class="trace-eyebrow">Execution trace</div>
          <h2>${escapeHtml(shortId(trace.request_id))}</h2>
          <code class="trace-full-id">${escapeHtml(trace.request_id)}</code>
        </div>
        <span class="trace-status large ${statusTone(trace.status)}">${escapeHtml(trace.status)}</span>
      </div>

      <div class="trace-summary-grid">
        ${summaryCell("Requested model", trace.requested_model || "unknown")}
        ${summaryCell("Selected route", selectedRoute)}
        ${summaryCell("Attempts", String(attempts.length))}
        ${summaryCell("Upstream time", `${totalDuration.toLocaleString()} ms`)}
        ${summaryCell("Started", formatTime(trace.started_at, true))}
        ${summaryCell("Result", terminal)}
      </div>

      ${trace.preferred_route ? `<div class="trace-context-note"><strong>Preferred route:</strong> ${escapeHtml(trace.preferred_route)}</div>` : ""}

      <div class="trace-timeline-heading">
        <div>
          <div class="trace-eyebrow">Attempt timeline</div>
          <h3>${attempts.length ? `${attempts.length} route attempt${attempts.length === 1 ? "" : "s"}` : "No upstream attempt"}</h3>
        </div>
      </div>

      <div class="trace-timeline">
        ${attempts.length ? attempts.map(attemptHtml).join("") : '<div class="trace-empty">The router could not produce an eligible route, so no upstream call was made.</div>'}
      </div>

      ${trace.final_error ? `<div class="trace-final-error"><div class="trace-eyebrow">Final error</div><pre>${escapeHtml(trace.final_error)}</pre></div>` : ""}`;
  }

  function summaryCell(label, value) {
    return `<div class="trace-summary-cell"><span>${escapeHtml(label)}</span><strong title="${escapeAttr(value)}">${escapeHtml(value)}</strong></div>`;
  }

  function attemptHtml(attempt, index) {
    const status = attempt.status_code == null ? "network" : String(attempt.status_code);
    const success = attempt.outcome === "success";
    const tone = success ? "success" : attemptTone(attempt);
    const retry = attempt.retryable ? '<span class="trace-mini-chip retry">retryable</span>' : "";
    const error = attempt.error
      ? `<details class="trace-attempt-error"><summary>Error detail</summary><pre>${escapeHtml(attempt.error)}</pre></details>`
      : "";
    return `<article class="trace-attempt ${tone}">
      <div class="trace-attempt-rail"><span>${index + 1}</span></div>
      <div class="trace-attempt-body">
        <div class="trace-attempt-top">
          <div>
            <div class="trace-attempt-route">${escapeHtml(attempt.route_id)}</div>
            <div class="trace-attempt-account">${escapeHtml(attempt.account_id)} · ${escapeHtml(attempt.model)}</div>
          </div>
          <div class="trace-attempt-badges">
            <span class="trace-http-chip ${tone}">${escapeHtml(status)}</span>
            <span class="trace-mini-chip">${escapeHtml(attempt.outcome)}</span>
            ${retry}
          </div>
        </div>
        <div class="trace-attempt-meta">
          <span>${Number(attempt.duration_ms || 0).toLocaleString()} ms</span>
          <span>${escapeHtml(formatTime(attempt.created_at, true))}</span>
        </div>
        ${error}
      </div>
    </article>`;
  }

  function renderDetailLoading() {
    const detail = document.getElementById("traceDetail");
    if (!detail) return;
    if (!selectedTrace || selectedTrace.request_id !== selectedRequestId) {
      detail.innerHTML = '<div class="trace-empty trace-detail-empty">Loading execution timeline…</div>';
    }
  }

  function statusTone(status) {
    if (status === "success") return "success";
    if (status === "failed") return "danger";
    if (status === "running") return "working";
    return "muted";
  }

  function attemptTone(attempt) {
    if (attempt.status_code === 429 || attempt.outcome === "rate_limited") return "warning";
    if ([401, 403].includes(Number(attempt.status_code)) || attempt.outcome === "authentication_error") return "danger";
    if (attempt.outcome === "transport_error") return "transport";
    return "danger";
  }

  function shortId(value) {
    const text = String(value || "");
    return text.length > 20 ? `${text.slice(0, 12)}…${text.slice(-6)}` : text;
  }

  function formatTime(value, includeDate = false) {
    if (!value) return "unknown";
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) return value;
    const options = includeDate
      ? { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit" }
      : { hour: "2-digit", minute: "2-digit", second: "2-digit" };
    return new Intl.DateTimeFormat(undefined, options).format(date);
  }

  async function errorText(response) {
    const text = await response.text();
    try { return JSON.parse(text)?.error?.message || `HTTP ${response.status}`; }
    catch (_) { return text || `HTTP ${response.status}`; }
  }

  function escapeHtml(value) {
    return String(value ?? "").replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;").replaceAll("'", "&#039;");
  }

  function escapeAttr(value) {
    return escapeHtml(value).replaceAll("`", "&#096;");
  }

  searchInput.addEventListener("input", render);
  refreshButton.addEventListener("click", () => load(true));
  document.addEventListener("click", (event) => {
    if (event.target.closest?.('.nav-button[data-view="traces"]')) setTimeout(() => load(true), 0);
  });

  setInterval(() => {
    if (active()) load();
  }, 5_000);

  if (active()) load(true);
  globalThis.llmgatewayTraceConsole = { load, loadDetail };
})();
