(() => {
  const LOCAL_KEY = "llmgateway.apiKey.local";
  const SESSION_KEY = "llmgateway.apiKey.session";
  const accountsContent = document.getElementById("accountsContent");
  const addButton = document.getElementById("addBrowserAccountButton");
  const wizardModal = document.getElementById("browserAccountModal");
  const wizardBody = document.getElementById("browserAccountWizardBody");
  const wizardResult = document.getElementById("browserAccountWizardResult");
  const wizardError = document.getElementById("browserAccountWizardError");
  const createButton = document.getElementById("createBrowserAccountButton");
  if (!accountsContent) return;

  let loading = false;
  let sessions = [];
  let driverState = new Map();
  let accountState = new Map();
  let refreshTimer = null;
  let loginPollTimer = null;
  let providerPresets = [];
  let selectedProvider = "gemini";

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
      const [summary, accounts] = await Promise.all([
        request("/_llmgateway/browser-sessions"),
        request("/_llmgateway/accounts"),
      ]);
      sessions = summary?.sessions || [];
      accountState = new Map((accounts?.data || []).map((account) => [account.id, account]));
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
      ${sessions.length
        ? `<div class="browser-session-grid">${sessions.map((session) => browserSessionHtml(session, driverState.get(session.id))).join("")}</div>`
        : browserEmptyHtml(summary)}
    `;

    const usage = accountsContent.querySelector(".usage-overview");
    const grid = accountsContent.querySelector(".account-grid");
    if (usage) accountsContent.insertBefore(panel, usage);
    else if (grid) accountsContent.insertBefore(panel, grid);
    else accountsContent.prepend(panel);
  }

  function browserEmptyHtml(summary) {
    return `
      <div class="browser-empty-state">
        <div class="browser-empty-icon">◎</div>
        <div>
          <strong>No browser accounts yet</strong>
          <p>Create a ChatGPT, Gemini, Qwen, or DeepSeek browser account. llmgateway will generate the linked session, provider, route, and isolated profile configuration for you.</p>
          <button type="button" class="browser-primary-action" data-open-browser-wizard>+ Add browser account</button>
        </div>
        <span class="browser-empty-meta">${summary?.profile_root ? `Profiles: ${escapeHtml(summary.profile_root)}` : "Isolated profiles"}</span>
      </div>`;
  }

  function browserSessionHtml(session, driver) {
    const account = accountState.get(session.id);
    const accountEnabled = account?.enabled !== false;
    const lifecycle = accountEnabled ? lifecycleView(session.status) : { tone: "idle", label: "Disabled" };
    const running = Boolean(driver?.status?.running);
    const driverReady = Boolean(driver?.status?.ready_match);
    const driverAvailable = Boolean(driver?.available);
    const button = primaryAction(session, driver, accountEnabled);
    const detail = sessionDetail(session, driver, accountEnabled);
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
        ${session.last_error ? `<div class="browser-session-error" title="${escapeAttr(session.last_error)}">${escapeHtml(shorten(session.last_error, 320))}</div>` : ""}
        ${driver?.error && !driverAvailable ? `<div class="browser-driver-note">${escapeHtml(shorten(driver.error, 150))}</div>` : ""}
        <div class="browser-session-actions">
          <button type="button" class="browser-primary-action" data-browser-action="${button.action}" data-session-id="${escapeAttr(session.id)}" ${button.disabled ? "disabled" : ""}>${escapeHtml(button.label)}</button>
          ${account ? `<button type="button" class="browser-secondary-action" data-browser-action="${accountEnabled ? "disable-account" : "enable-account"}" data-session-id="${escapeAttr(session.id)}">${accountEnabled ? "Disable account" : "Enable account"}</button>` : ""}
          ${accountEnabled ? `<button type="button" class="browser-secondary-action" data-browser-action="reauth" data-session-id="${escapeAttr(session.id)}">Re-authenticate</button>` : ""}
          ${running && accountEnabled ? `<button type="button" class="browser-secondary-action" data-browser-action="restart" data-session-id="${escapeAttr(session.id)}">Restart browser</button>` : ""}
          ${running ? `<button type="button" class="browser-secondary-action" data-browser-action="stop" data-session-id="${escapeAttr(session.id)}">Stop browser</button>` : ""}
          ${["requires_attention", "failed"].includes(session.status) ? `<button type="button" class="browser-secondary-action" data-browser-action="reset" data-session-id="${escapeAttr(session.id)}">Reset</button>` : ""}
        </div>
      </article>`;
  }

  function primaryAction(session, driver, accountEnabled = true) {
    if (!accountEnabled) return { action: "none", label: "Account disabled", disabled: true };
    if (!session.enabled) return { action: "none", label: "Session disabled", disabled: true };
    if (!driver?.available) return { action: "none", label: "Driver unavailable", disabled: true };
    if (session.status === "starting" || (session.status === "login_required" && driver.status?.running)) return { action: "verify", label: "Check login", disabled: false };
    if (session.status === "ready") {
      return driver.status?.running
        ? { action: "verify", label: "Verify session", disabled: false }
        : { action: "launch", label: "Open session", disabled: false };
    }
    return { action: "launch", label: "Login with browser", disabled: false };
  }

  function sessionDetail(session, driver, accountEnabled = true) {
    if (!accountEnabled) return "Routing is disabled for this account. The isolated Chromium profile is preserved.";
    if (!session.enabled) return "This browser session is disabled in configuration.";
    if (session.status === "starting" || session.status === "login_required") {
      return driver?.status?.running
        ? "Finish the normal login flow in the Chromium window. This page will detect completion automatically."
        : "Start the isolated Chromium profile and finish the normal provider login.";
    }
    if (session.status === "ready") {
      return session.last_verified_at
        ? `Connected · last verified ${relativeTime(session.last_verified_at)}`
        : "Connected and ready for a browser-backed provider adapter.";
    }
    if (session.status === "degraded") return "The saved browser session is temporarily unavailable; llmgateway will try safe automatic recovery.";
    if (session.status === "stopped") return "The browser was stopped intentionally. The isolated profile is preserved for the next launch.";
    if (session.status === "failed") return "Browser launch or recovery failed. Review the diagnostic below, reset the session, then try again.";
    if (session.status === "requires_attention") return "The session needs attention. Reset it, then start a normal browser login again.";
    return "Start a dedicated Chromium profile and sign in normally. CAPTCHA and 2FA stay interactive.";
  }

  function lifecycleView(status) {
    switch (status) {
      case "ready": return { tone: "ready", label: "Connected" };
      case "starting": return { tone: "working", label: "Starting" };
      case "login_required": return { tone: "working", label: "Login required" };
      case "degraded": return { tone: "attention", label: "Recovering" };
      case "failed": return { tone: "attention", label: "Browser error" };
      case "stopped": return { tone: "idle", label: "Stopped" };
      case "requires_attention": return { tone: "attention", label: "Needs attention" };
      default: return { tone: "idle", label: "Not connected" };
    }
  }

  async function openWizard() {
    if (!wizardModal) return;
    resetWizard();
    wizardModal.classList.remove("hidden");
    try {
      const result = await request("/_llmgateway/browser-account-setup/providers");
      providerPresets = result?.providers || [];
      renderProviderPresets();
    } catch (error) {
      showWizardError(cleanError(error));
    }
    requestAnimationFrame(() => document.getElementById("browserAccountIdInput")?.focus());
  }

  function resetWizard() {
    selectedProvider = "gemini";
    providerPresets = [];
    wizardBody?.classList.remove("hidden");
    wizardResult?.classList.add("hidden");
    if (wizardResult) wizardResult.innerHTML = "";
    hideWizardError();
    const values = {
      browserAccountIdInput: "",
      browserAccountLabelInput: "",
      browserModelIdInput: "",
      browserModelLabelInput: "",
      browserPriorityInput: "10",
    };
    for (const [id, value] of Object.entries(values)) {
      const input = document.getElementById(id);
      if (input) input.value = value;
    }
    renderProviderPresets();
  }

  function renderProviderPresets() {
    const picker = document.getElementById("browserProviderPicker");
    if (!picker) return;
    const fallback = [
      { id: "chatgpt", label: "ChatGPT Web", default_model_id: "chatgpt-web-default" },
      { id: "gemini", label: "Gemini Web", default_model_id: "gemini-web-default" },
      { id: "qwen", label: "Qwen Web", default_model_id: "qwen-web-default" },
      { id: "deepseek", label: "DeepSeek Web", default_model_id: "deepseek-web-default" },
    ];
    const presets = providerPresets.length ? providerPresets : fallback;
    picker.innerHTML = presets.map((preset) => `
      <button type="button" class="browser-provider-option ${preset.id === selectedProvider ? "selected" : ""}" data-browser-provider="${escapeAttr(preset.id)}" aria-pressed="${preset.id === selectedProvider}">
        <span class="browser-provider-option-mark">${escapeHtml(String(preset.label || preset.id).slice(0, 1).toUpperCase())}</span>
        <span><strong>${escapeHtml(preset.label || preset.id)}</strong><small>${escapeHtml(preset.default_model_id || "Browser-backed model")}</small></span>
      </button>`).join("");
  }

  function selectProvider(provider) {
    selectedProvider = provider;
    renderProviderPresets();
    const preset = providerPresets.find((item) => item.id === provider);
    const modelInput = document.getElementById("browserModelIdInput");
    if (modelInput && !modelInput.value.trim() && preset?.default_model_id) {
      modelInput.placeholder = preset.default_model_id;
    }
  }

  async function createBrowserAccount() {
    if (!createButton) return;
    hideWizardError();
    setBusy(createButton, "Creating…");
    try {
      const priorityValue = document.getElementById("browserPriorityInput")?.value;
      const payload = {
        provider: selectedProvider,
        account_id: optionalValue("browserAccountIdInput"),
        label: optionalValue("browserAccountLabelInput"),
        model_id: optionalValue("browserModelIdInput"),
        model_label: optionalValue("browserModelLabelInput"),
        priority: priorityValue === "" ? null : Number(priorityValue),
      };
      const result = await request("/_llmgateway/browser-account-setup", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
      showWizardResult(result);
      scheduleRefresh(40);
      browserToast(`${result.account_id} added and activated`);
    } catch (error) {
      showWizardError(cleanError(error));
    } finally {
      clearBusy(createButton);
    }
  }

  function optionalValue(id) {
    const value = document.getElementById(id)?.value?.trim() || "";
    return value || null;
  }

  function showWizardResult(result) {
    wizardBody?.classList.add("hidden");
    wizardResult?.classList.remove("hidden");
    if (!wizardResult) return;
    const steps = Array.isArray(result?.next_steps) ? result.next_steps : [];
    wizardResult.innerHTML = `
      <div class="browser-wizard-success-mark">✓</div>
      <div class="browser-wizard-success-copy">
        <div class="browser-wizard-eyebrow">Configuration created</div>
        <h3>${escapeHtml(result?.account_id || "Browser account")} is ready</h3>
        <p>llmgateway created one linked browser session, provider account, route, and isolated Chromium profile configuration.</p>
      </div>
      <div class="browser-wizard-summary">
        <div><span>Provider</span><strong>${escapeHtml(result?.provider || selectedProvider)}</strong></div>
        <div><span>Model</span><strong>${escapeHtml(result?.model_id || "")}</strong></div>
        <div><span>Route</span><strong>${escapeHtml(result?.route_id || "")}</strong></div>
        <div><span>Config</span><strong title="${escapeAttr(result?.config_path || "")}">${escapeHtml(shorten(result?.config_path || "", 42))}</strong></div>
      </div>
      ${result?.restart_required ? `
        <div class="browser-restart-notice">
          <strong>One restart required in this v0.29 phase</strong>
          <span>The config is already saved and validated. Restart llmgateway once, then open Accounts and click Login with browser. Hot activation is the next implementation slice.</span>
        </div>` : ""}
      ${steps.length ? `<ol class="browser-wizard-next-steps">${steps.map((step) => `<li>${escapeHtml(step)}</li>`).join("")}</ol>` : ""}
      <div class="browser-wizard-actions">
        <button type="button" class="secondary-button" data-close-modal="browserAccountModal">Close</button>
        <button type="button" class="secondary-button" data-create-another-browser-account>Add another</button>
        ${result?.restart_required ? "" : `<button type="button" class="primary-button" data-browser-action="launch" data-session-id="${escapeAttr(result?.session_id || result?.account_id || "")}">Login with browser</button>`}
      </div>`;
  }

  function showWizardError(message) {
    if (!wizardError) return;
    wizardError.textContent = message;
    wizardError.classList.remove("hidden");
  }

  function hideWizardError() {
    if (!wizardError) return;
    wizardError.textContent = "";
    wizardError.classList.add("hidden");
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
        window.dispatchEvent(new CustomEvent("llmgateway:models-changed", {
          detail: { sessionId }
        }));
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

  async function setAccountEnabled(sessionId, enabled, button) {
    setBusy(button, enabled ? "Enabling…" : "Disabling…");
    try {
      if (!enabled && driverState.get(sessionId)?.status?.running) {
        try {
          await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/driver/stop`, { method: "POST" });
        } catch (_) {}
      }
      await request(`/_llmgateway/browser-account-setup/${encodeURIComponent(sessionId)}`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled }),
      });
      browserToast(`${sessionId} ${enabled ? "enabled" : "disabled"}`);
    } catch (error) {
      browserToast(`${enabled ? "Enable" : "Disable"} failed: ${cleanError(error)}`, true);
    } finally {
      clearBusy(button);
      await loadBrowserSessions(true);
    }
  }

  async function reauthenticate(sessionId, button) {
    setBusy(button, "Opening login…");
    try {
      if (driverState.get(sessionId)?.status?.running) {
        try {
          await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/driver/stop`, { method: "POST" });
        } catch (_) {}
      }
      await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/reset`, { method: "POST" });
      await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/driver/launch`, { method: "POST" });
      browserToast(`Re-authentication opened for ${sessionId}`);
      startLoginPolling();
    } catch (error) {
      browserToast(`Re-authentication failed: ${cleanError(error)}`, true);
    } finally {
      clearBusy(button);
      await loadBrowserSessions(true);
    }
  }

  async function restartBrowser(sessionId, button) {
    setBusy(button, "Restarting…");
    try {
      if (driverState.get(sessionId)?.status?.running) {
        await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/driver/stop`, { method: "POST" });
      }
      await request(`/_llmgateway/browser-sessions/${encodeURIComponent(sessionId)}/driver/launch`, { method: "POST" });
      browserToast(`Browser restarted for ${sessionId}`);
      startLoginPolling();
    } catch (error) {
      browserToast(`Restart failed: ${cleanError(error)}`, true);
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
    const pending = sessions.some((session) => ["starting", "login_required"].includes(session.status) && Boolean(driverState.get(session.id)?.status?.running));
    if (pending && isAccountsViewActive()) startLoginPolling();
    else stopLoginPolling();
  }

  async function pollPendingLogins() {
    if (!isAccountsViewActive() || loading) return;
    const pending = sessions.filter((session) => ["starting", "login_required"].includes(session.status) && Boolean(driverState.get(session.id)?.status?.running));
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
    if (event.target.closest?.("#addBrowserAccountButton") || event.target.closest?.("[data-open-browser-wizard]")) {
      openWizard();
      return;
    }
    const provider = event.target.closest?.("[data-browser-provider]");
    if (provider) {
      selectProvider(provider.dataset.browserProvider);
      return;
    }
    if (event.target.closest?.("#createBrowserAccountButton")) {
      createBrowserAccount();
      return;
    }
    if (event.target.closest?.("[data-create-another-browser-account]")) {
      resetWizard();
      return;
    }

    const actionButton = event.target.closest?.("[data-browser-action]");
    if (actionButton) {
      const sessionId = actionButton.dataset.sessionId;
      switch (actionButton.dataset.browserAction) {
        case "launch": launch(sessionId, actionButton); break;
        case "verify": verify(sessionId, actionButton); break;
        case "stop": stop(sessionId, actionButton); break;
        case "reset": reset(sessionId, actionButton); break;
        case "disable-account": setAccountEnabled(sessionId, false, actionButton); break;
        case "enable-account": setAccountEnabled(sessionId, true, actionButton); break;
        case "reauth": reauthenticate(sessionId, actionButton); break;
        case "restart": restartBrowser(sessionId, actionButton); break;
      }
      return;
    }
    if (event.target.closest?.('.nav-button[data-view="accounts"]')) scheduleRefresh(100);
    if (event.target.closest?.("#refreshAccountsButton")) scheduleRefresh(220);
  });

  wizardModal?.addEventListener("click", (event) => {
    if (event.target === wizardModal) wizardModal.classList.add("hidden");
  });

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && wizardModal && !wizardModal.classList.contains("hidden")) {
      wizardModal.classList.add("hidden");
    }
  });

  const observer = new MutationObserver(() => {
    if (!isAccountsViewActive()) return;
    if (!accountsContent.querySelector(".browser-control-panel")) scheduleRefresh(60);
  });
  observer.observe(accountsContent, { childList: true });

  setInterval(() => {
    if (isAccountsViewActive() && !loginPollTimer) loadBrowserSessions();
  }, 15_000);

  if (isAccountsViewActive()) scheduleRefresh();

  globalThis.llmgatewayBrowserUi = {
    loadBrowserSessions,
    lifecycleView,
    openWizard,
    createBrowserAccount,
  };
})();
