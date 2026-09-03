(() => {
  const LOCAL_KEY = "llmgateway.apiKey.local";
  const SESSION_KEY = "llmgateway.apiKey.session";
  const accountsContent = document.getElementById("accountsContent");
  if (!accountsContent) return;

  let loading = false;
  let sessions = [];
  let driverState = new Map();
  let refreshTimer = null;
  let loginPollTimer = null;

  function apiKey() {
    return localStorage.getItem(LOCAL_KEY) || sessionStorage.getItem(SESSION_KEY) || "";
  }

  function authHeaders() {
    const key = apiKey();
    return key ? { Authorization: `Bearer ${key}` } : {};
  }

  function isAccountsViewActive() {
    return document.getElementById("accountsView")?.classList.contains("active-view") === true;
  }

  async function request(path, options = {}) {
    const response = await fetch(path, {
      ...options,
      headers: { ...authHeaders(), ...(options.headers || {}) },
    });
    const raw = await response.text();
    let body = null;
    if (raw) {
      try { body = JSON.parse(raw); } catch (_) { body = raw; }
    }
    if (!response.ok) {
      const message = body?.error?.message || (typeof body === "string" ? body : `HTTP ${response.status}`);
      const error = new Error(message);
      error.status = response.status;
      throw error;
    }
    return body;
  }

  async function loadBrowserSessions(force = false) {
    if (!apiKey() || loading || (!force && !isAccountsViewActive())) return;
    loading = true;
    try {
      const summary = await request("/_llmgateway/browser-sessions");
      sessions = summary?.sessions || [];
      if (!sessions.length) {
        removePanel();
        stopLoginPolling();
        return;
      }
      const states = await Promise.all(sessions.map(async (session) => [session.id, await loadDriverStatus(session.id)]));
      driverState = new Map(states);
      render(summary);
      syncLoginPolling();
    } catch (error) {
      renderError(error);
    } finally {
      loading = false;
    }
  }

  async function loadDriverStatus(sessionId) {
    try {
      const status = await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/driver/status`);
      return { available: true, status, error: null };
    } catch (error) {
      return { available: false, status: null, error: cleanError(error) };
    }
  }

  function render(summary) {
    removePanel();
    const panel = document.createElement("section");
    panel.className = "browser-control-panel";
    panel.innerHTML = `
      <div class="browser-control-head">
        <div>
          <div class="browser-eyebrow">Browser accounts</div>
          <div class="browser-control-title">Sign in once. Keep the session local.</div>
          <div class="browser-control-subtitle">Chromium uses an isolated profile per account. Cookies stay inside that profile and are never returned by llmgateway.</div>
        </div>
        <div class="browser-security-chip" title="Browser DevTools is loopback-only and page URLs are sanitized before they reach this UI">Local session</div>
      </div>
      <div class="browser-session-grid">
        ${sessions.map((session) => browserSessionHtml(session, driverState.get(session.id))).join("")}
      </div>`;

    const usage = accountsContent.querySelector(".usage-overview");
    const grid = accountsContent.querySelector(".account-grid");
    if (usage) accountsContent.insertBefore(panel, usage);
    else if (grid) accountsContent.insertBefore(panel, grid);
    else accountsContent.prepend(panel);
  }

  function browserSessionHtml(session, driver) {
    const lifecycle = lifecycleView(session.status);
    const running = Boolean(driver?.status?.running);
    const driverReady = Boolean(driver?.status?.ready_match);
    const driverAvailable = Boolean(driver?.available);
    const button = primaryAction(session, driver);
    const detail = sessionDetail(session, driver);
    const providerMark = String(session.provider || "?").slice(0, 1).toUpperCase();

    return `
      <article class="browser-session-card" data-browser-session-card="${escapeAttr(session.id)}">
        <div class="browser-session-top">
          <div class="browser-provider-mark">${escapeHtml(providerMark)}</div>
          <div class="browser-session-identity">
            <div class="browser-session-name">${escapeHtml(session.label || session.id)}</div>
            <div class="browser-session-meta">${escapeHtml(session.provider)} · ${escapeHtml(session.id)}</div>
          </div>
          <span class="browser-status ${lifecycle.tone}"><span class="browser-status-dot"></span>${escapeHtml(lifecycle.label)}</span>
        </div>
        <div class="browser-session-detail">${escapeHtml(detail)}</div>
        <div class="browser-session-facts">
          <span>${running ? "Browser running" : "Browser stopped"}</span>
          <span>${driverReady ? "Authenticated page detected" : driverAvailable ? "Waiting for authenticated page" : "Chromium driver unavailable"}</span>
        </div>
        ${session.last_error ? `<div class="browser-session-error">${escapeHtml(shorten(session.last_error, 150))}</div>` : ""}
        ${driver?.error && !driverAvailable ? `<div class="browser-driver-note">${escapeHtml(shorten(driver.error, 150))}</div>` : ""}
        <div class="browser-session-actions">
          <button type="button" class="browser-primary-action" data-browser-action="${button.action}" data-session-id="${escapeAttr(session.id)}" ${button.disabled ? "disabled" : ""}>${escapeHtml(button.label)}</button>
          ${running ? `<button type="button" class="browser-secondary-action" data-browser-action="stop" data-session-id="${escapeAttr(session.id)}">Stop browser</button>` : ""}
          ${session.status === "requires_attention" ? `<button type="button" class="browser-secondary-action" data-browser-action="reset" data-session-id="${escapeAttr(session.id)}">Reset</button>` : ""}
        </div>
      </article>`;
  }

  function primaryAction(session, driver) {
    if (!session.enabled) return { action: "none", label: "Disabled", disabled: true };
    if (!driver?.available) return { action: "none", label: "Driver unavailable", disabled: true };
    if (session.status === "login_in_progress") return { action: "verify", label: "Check login", disabled: false };
    if (session.status === "ready") {
      return driver.status?.running
        ? { action: "verify", label: "Verify session", disabled: false }
        : { action: "launch", label: "Open session", disabled: false };
    }
    return { action: "launch", label: "Login with browser", disabled: false };
  }

  function sessionDetail(session, driver) {
    if (!session.enabled) return "This browser session is disabled in configuration.";
    if (session.status === "login_in_progress") {
      return driver?.status?.running
        ? "Finish the normal login flow in the Chromium window. This page will detect completion automatically."
        : "The login browser is no longer running. Check the session or start login again.";
    }
    if (session.status === "ready") {
      return session.last_verified_at
        ? `Connected · last verified ${relativeTime(session.last_verified_at)}`
        : "Connected and ready for a browser-backed provider adapter.";
    }
    if (session.status === "requires_attention") {
      return "The session needs attention. Reset it, then start a normal browser login again.";
    }
    return "Start a dedicated Chromium profile and sign in normally. CAPTCHA and 2FA stay interactive.";
  }

  function lifecycleView(status) {
    switch (status) {
      case "ready": return { tone: "ready", label: "Connected" };
      case "login_in_progress": return { tone: "working", label: "Signing in" };
      case "requires_attention": return { tone: "attention", label: "Needs attention" };
      default: return { tone: "idle", label: "Not connected" };
    }
  }

  async function launch(sessionId, button) {
    setBusy(button, "Opening…");
    try {
      await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/driver/launch`, { method: "POST" });
      browserToast(`Chromium opened for ${sessionId}. Finish login in the browser.`);
      await loadBrowserSessions(true);
      startLoginPolling();
    } catch (error) {
      browserToast(`Could not open browser: ${cleanError(error)}`, true);
      await loadBrowserSessions(true);
    } finally {
      clearBusy(button);
    }
  }

  async function verify(sessionId, button, quiet = false) {
    if (button) setBusy(button, "Checking…");
    try {
      const result = await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/driver/verify`, { method: "POST" });
      if (result?.authenticated) {
        if (!quiet) browserToast(`${sessionId} connected`);
        await loadBrowserSessions(true);
        return true;
      }
      if (!quiet) browserToast("Login is still waiting in the browser.");
      if (!quiet) await loadBrowserSessions(true);
      return false;
    } catch (error) {
      if (!quiet) browserToast(`Verification failed: ${cleanError(error)}`, true);
      if (!quiet) await loadBrowserSessions(true);
      return false;
    } finally {
      if (button) clearBusy(button);
    }
  }

  async function stop(sessionId, button) {
    setBusy(button, "Stopping…");
    try {
      await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/driver/stop`, { method: "POST" });
      browserToast(`Browser stopped for ${sessionId}`);
    } catch (error) {
      browserToast(`Stop failed: ${cleanError(error)}`, true);
    } finally {
      clearBusy(button);
      await loadBrowserSessions(true);
    }
  }

  async function reset(sessionId, button) {
    setBusy(button, "Resetting…");
    try {
      const driver = driverState.get(sessionId);
      if (driver?.status?.running) {
        try {
          await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/driver/stop`, { method: "POST" });
        } catch (_) {}
      }
      await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/reset`, { method: "POST" });
      browserToast(`${sessionId} reset. You can sign in again.`);
    } catch (error) {
      browserToast(`Reset failed: ${cleanError(error)}`, true);
    } finally {
      clearBusy(button);
      await loadBrowserSessions(true);
    }
  }

  function startLoginPolling() {
    if (loginPollTimer) return;
    loginPollTimer = setInterval(pollPendingLogins, 2200);
  }

  function stopLoginPolling() {
    clearInterval(loginPollTimer);
    loginPollTimer = null;
  }

  function syncLoginPolling() {
    const pending = sessions.some((session) => session.status === "login_in_progress");
    if (pending && isAccountsViewActive()) startLoginPolling();
    else stopLoginPolling();
  }

  async function pollPendingLogins() {
    if (!isAccountsViewActive() || loading) return;
    const pending = sessions.filter((session) => session.status === "login_in_progress");
    if (!pending.length) {
      stopLoginPolling();
      return;
    }
    let changed = false;
    for (const session of pending) {
      try {
        const result = await request(`/_llmgateway/browser-sessions/${encodeURIComponent(session.id)}/driver/verify`, { method: "POST" });
        if (result?.authenticated || result?.status?.running === false) changed = true;
      } catch (_) {
        changed = true;
      }
    }
    if (changed) await loadBrowserSessions(true);
  }

  function setBusy(button, label) {
    if (!button) return;
    if (!button.dataset.originalLabel) button.dataset.originalLabel = button.textContent;
    button.disabled = true;
    button.textContent = label;
  }

  function clearBusy(button) {
    if (!button) return;
    button.disabled = false;
    if (button.dataset.originalLabel) {
      button.textContent = button.dataset.originalLabel;
      delete button.dataset.originalLabel;
    }
  }

  function removePanel() {
    accountsContent.querySelector(".browser-control-panel")?.remove();
  }

  function renderError(error) {
    removePanel();
    if (!accountsContent.querySelector(".account-grid")) return;
    const panel = document.createElement("section");
    panel.className = "browser-control-panel browser-control-error";
    panel.innerHTML = `
      <div class="browser-control-head">
        <div>
          <div class="browser-eyebrow">Browser accounts</div>
          <div class="browser-control-title">Browser session status unavailable</div>
          <div class="browser-control-subtitle">${escapeHtml(cleanError(error))}</div>
        </div>
      </div>`;
    accountsContent.prepend(panel);
  }

  function scheduleRefresh(delay = 80) {
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(() => loadBrowserSessions(true), delay);
  }

  function relativeTime(value) {
    const time = Date.parse(value);
    if (!Number.isFinite(time)) return "recently";
    const diff = Date.now() - time;
    if (diff < 60_000) return "just now";
    if (diff < 3_600_000) return `${Math.max(1, Math.round(diff / 60_000))}m ago`;
    if (diff < 86_400_000) return `${Math.max(1, Math.round(diff / 3_600_000))}h ago`;
    return `${Math.max(1, Math.round(diff / 86_400_000))}d ago`;
  }

  function shorten(value, max) {
    const text = String(value || "").replace(/\s+/g, " ").trim();
    return text.length > max ? `${text.slice(0, max - 1)}…` : text;
  }

  function cleanError(error) {
    return error?.message || String(error || "Unknown error");
  }

  function escapeHtml(value) {
    return String(value ?? "").replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;").replaceAll("'", "&#039;");
  }

  function escapeAttr(value) {
    return escapeHtml(value).replaceAll("`", "&#096;");
  }

  function browserToast(message, danger = false) {
    let node = document.getElementById("browserControlToast");
    if (!node) {
      node = document.createElement("div");
      node.id = "browserControlToast";
      node.className = "browser-control-toast";
      document.body.appendChild(node);
    }
    node.textContent = message;
    node.classList.toggle("danger", danger);
    node.classList.add("visible");
    clearTimeout(browserToast.timer);
    browserToast.timer = setTimeout(() => node.classList.remove("visible"), 3600);
  }

  document.addEventListener("click", (event) => {
    const actionButton = event.target.closest?.("[data-browser-action]");
    if (actionButton) {
      const sessionId = actionButton.dataset.sessionId;
      switch (actionButton.dataset.browserAction) {
        case "launch": launch(sessionId, actionButton); break;
        case "verify": verify(sessionId, actionButton); break;
        case "stop": stop(sessionId, actionButton); break;
        case "reset": reset(sessionId, actionButton); break;
      }
      return;
    }
    if (event.target.closest?.('.nav-button[data-view="accounts"]')) scheduleRefresh(100);
    if (event.target.closest?.("#refreshAccountsButton")) scheduleRefresh(220);
  });

  const observer = new MutationObserver(() => {
    if (!isAccountsViewActive()) return;
    if (accountsContent.querySelector(".account-grid") && !accountsContent.querySelector(".browser-control-panel")) {
      scheduleRefresh(60);
    }
  });
  observer.observe(accountsContent, { childList: true });

  setInterval(() => {
    if (isAccountsViewActive() && !loginPollTimer) loadBrowserSessions();
  }, 15_000);

  if (isAccountsViewActive()) scheduleRefresh();

  globalThis.llmgatewayBrowserUi = { loadBrowserSessions, lifecycleView };
})();
