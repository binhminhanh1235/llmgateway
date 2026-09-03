(() => {
  const LOCAL_KEY = "llmgateway.apiKey.local";
  const SESSION_KEY = "llmgateway.apiKey.session";
  const accountsContent = document.getElementById("accountsContent");
  if (!accountsContent) return;

  let loading = false;
  let lastSummary = null;
  let refreshTimer = null;

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

  async function loadUsage(force = false) {
    if (!apiKey() || loading || (!force && !isAccountsViewActive())) return;
    loading = true;
    try {
      const response = await fetch("/_llmgateway/usage", { headers: authHeaders() });
      if (!response.ok) throw new Error(await response.text() || `HTTP ${response.status}`);
      lastSummary = await response.json();
      renderUsage(lastSummary);
    } catch (error) {
      renderUsageError(error);
    } finally {
      loading = false;
    }
  }

  function renderUsage(summary) {
    const cards = [...accountsContent.querySelectorAll(".account-card")];
    if (!cards.length) return;

    accountsContent.querySelector(".usage-overview")?.remove();
    const snapshots = new Map((summary.accounts || []).map((item) => [item.account_id, item]));
    const statuses = (summary.accounts || []).map(classifyUsage);
    const healthy = statuses.filter((status) => status.level === "healthy").length;
    const constrained = statuses.filter((status) => status.level !== "healthy").length;
    const dailyRequests = (summary.accounts || []).reduce((sum, item) => sum + Number(item.daily?.requests || 0), 0);
    const monthlyRequests = (summary.accounts || []).reduce((sum, item) => sum + Number(item.monthly?.requests || 0), 0);

    const overview = document.createElement("section");
    overview.className = "usage-overview";
    overview.innerHTML = `
      <div class="usage-overview-copy">
        <div class="usage-eyebrow">Quota control plane</div>
        <div class="usage-overview-title">${summary.enabled ? `${healthy} healthy · ${constrained} constrained` : "Usage tracking disabled"}</div>
        <div class="usage-overview-subtitle">Persistent account-level quota state. Routing automatically avoids blocked accounts.</div>
      </div>
      <div class="usage-overview-metrics">
        ${metricHtml("Today", `${formatNumber(dailyRequests)} req`)}
        ${metricHtml("This month", `${formatNumber(monthlyRequests)} req`)}
        ${metricHtml("Enforcement", summary.hard_limits ? "Hard limits" : "Observe only")}
      </div>`;

    const grid = accountsContent.querySelector(".account-grid");
    if (grid) accountsContent.insertBefore(overview, grid);
    else accountsContent.prepend(overview);

    for (const card of cards) {
      const accountId = card.querySelector(".account-name")?.textContent?.trim();
      if (!accountId) continue;
      decorateAccountCard(card, snapshots.get(accountId), summary.enabled);
    }
  }

  function decorateAccountCard(card, snapshot, usageEnabled) {
    card.querySelector(".quota-panel")?.remove();
    const panel = document.createElement("section");
    panel.className = "quota-panel";

    if (!usageEnabled) {
      panel.innerHTML = '<div class="quota-empty">Usage tracking is disabled in <code>[usage]</code>.</div>';
    } else if (!snapshot) {
      panel.innerHTML = '<div class="quota-empty">No usage state has been recorded for this account yet.</div>';
    } else {
      const status = classifyUsage(snapshot);
      const hints = [];
      if (snapshot.remaining_requests_hint != null) hints.push(`${formatNumber(snapshot.remaining_requests_hint)} req remaining upstream`);
      if (snapshot.remaining_tokens_hint != null) hints.push(`${formatNumber(snapshot.remaining_tokens_hint)} tokens remaining upstream`);
      if (snapshot.consecutive_429) hints.push(`${snapshot.consecutive_429} consecutive 429`);

      panel.innerHTML = `
        <div class="quota-status-row">
          <span class="quota-status ${status.level}"><span class="quota-status-dot"></span>${escapeHtml(status.label)}</span>
          <span class="quota-status-note">${escapeHtml(status.note)}</span>
        </div>
        <div class="quota-windows">
          ${windowHtml("Today", snapshot.daily)}
          ${windowHtml("This month", snapshot.monthly)}
        </div>
        ${hints.length ? `<div class="quota-hints">${hints.map((hint) => `<span>${escapeHtml(hint)}</span>`).join("")}</div>` : ""}
        ${snapshot.last_error ? `<div class="quota-last-error" title="${escapeAttr(snapshot.last_error)}">Last upstream error: ${escapeHtml(shorten(snapshot.last_error, 120))}</div>` : ""}
        <div class="quota-actions">
          <span class="quota-updated">${snapshot.last_429_at ? `Last 429 ${escapeHtml(relativeTime(snapshot.last_429_at))}` : "No recent rate-limit event"}</span>
          <button type="button" class="quota-reset-button" data-reset-quota="${escapeAttr(snapshot.account_id)}" ${snapshot.blocked || snapshot.consecutive_429 || snapshot.cooldown_until ? "" : "disabled"}>Reset quota state</button>
        </div>`;
    }

    const models = card.querySelector(".account-models");
    if (models) card.insertBefore(panel, models);
    else card.appendChild(panel);
  }

  function classifyUsage(snapshot) {
    const pressure = Math.max(Number(snapshot.daily?.pressure || 0), Number(snapshot.monthly?.pressure || 0));
    const now = Date.now();
    const cooldownUntil = snapshot.cooldown_until ? Date.parse(snapshot.cooldown_until) : 0;
    if (snapshot.blocked && cooldownUntil > now) {
      return { level: "blocked", label: "Cooling down", note: `retry ${relativeTime(snapshot.cooldown_until)}` };
    }
    if (snapshot.blocked) return { level: "blocked", label: "Blocked", note: "router will skip this account" };
    if (snapshot.remaining_requests_hint === 0 || snapshot.remaining_tokens_hint === 0) {
      return { level: "warning", label: "Upstream limited", note: "remaining quota hint reached zero" };
    }
    if (pressure >= 1) return { level: "blocked", label: "At budget", note: "configured usage budget reached" };
    if (pressure >= 0.8) return { level: "warning", label: "Near budget", note: `${Math.round(pressure * 100)}% pressure` };
    if (snapshot.consecutive_429 > 0) return { level: "warning", label: "Recovering", note: "recent rate-limit response" };
    return { level: "healthy", label: "Healthy", note: pressure > 0 ? `${Math.round(pressure * 100)}% pressure` : "ready for routing" };
  }

  function windowHtml(label, usage = {}) {
    const pressure = Math.max(0, Number(usage.pressure || 0));
    const percent = Math.min(100, Math.round(pressure * 100));
    const req = limitText(usage.requests, usage.request_limit, "req");
    const tokens = limitText(usage.tokens, usage.token_limit, "tok");
    const tone = percent >= 100 ? "blocked" : percent >= 80 ? "warning" : "healthy";
    return `
      <div class="quota-window">
        <div class="quota-window-head"><span>${escapeHtml(label)}</span><strong>${percent}%</strong></div>
        <div class="quota-progress"><span class="${tone}" style="width:${percent}%"></span></div>
        <div class="quota-window-values"><span>${escapeHtml(req)}</span><span>${escapeHtml(tokens)}</span></div>
      </div>`;
  }

  function metricHtml(label, value) {
    return `<div class="usage-metric"><span>${escapeHtml(label)}</span><strong>${escapeHtml(value)}</strong></div>`;
  }

  function limitText(value, limit, suffix) {
    const used = formatNumber(value || 0);
    return limit == null ? `${used} ${suffix}` : `${used} / ${formatNumber(limit)} ${suffix}`;
  }

  async function resetQuota(accountId, button) {
    if (!accountId || !apiKey()) return;
    const old = button.textContent;
    button.disabled = true;
    button.textContent = "Resetting…";
    try {
      const response = await fetch(`/_llmgateway/accounts/${encodeURIComponent(accountId)}/quota/reset`, {
        method: "POST",
        headers: authHeaders(),
      });
      if (!response.ok) throw new Error(await response.text() || `HTTP ${response.status}`);
      controlToast(`Quota state reset for ${accountId}`);
      await loadUsage(true);
    } catch (error) {
      controlToast(`Reset failed: ${cleanError(error)}`, true);
    } finally {
      button.disabled = false;
      button.textContent = old;
    }
  }

  function renderUsageError(error) {
    const existing = accountsContent.querySelector(".usage-overview");
    if (existing) {
      existing.classList.add("usage-overview-error");
      existing.querySelector(".usage-overview-title").textContent = "Quota telemetry unavailable";
      existing.querySelector(".usage-overview-subtitle").textContent = cleanError(error);
    }
  }

  function relativeTime(value) {
    const time = Date.parse(value);
    if (!Number.isFinite(time)) return "recently";
    const diff = time - Date.now();
    const abs = Math.abs(diff);
    if (abs < 60_000) return diff >= 0 ? "in <1m" : "<1m ago";
    if (abs < 3_600_000) {
      const minutes = Math.max(1, Math.round(abs / 60_000));
      return diff >= 0 ? `in ${minutes}m` : `${minutes}m ago`;
    }
    const hours = Math.max(1, Math.round(abs / 3_600_000));
    return diff >= 0 ? `in ${hours}h` : `${hours}h ago`;
  }

  function formatNumber(value) {
    const number = Number(value || 0);
    if (!Number.isFinite(number)) return "0";
    if (Math.abs(number) >= 1_000_000) return `${(number / 1_000_000).toFixed(number >= 10_000_000 ? 0 : 1)}M`;
    if (Math.abs(number) >= 1_000) return `${(number / 1_000).toFixed(number >= 10_000 ? 0 : 1)}k`;
    return Math.round(number).toLocaleString();
  }

  function shorten(value, max) {
    const text = String(value || "").replace(/\s+/g, " ").trim();
    return text.length > max ? `${text.slice(0, max - 1)}…` : text;
  }

  function cleanError(error) {
    const raw = error?.message || String(error || "Unknown error");
    try {
      const parsed = JSON.parse(raw);
      return parsed?.error?.message || raw;
    } catch (_) {
      return raw;
    }
  }

  function escapeHtml(value) {
    return String(value ?? "").replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;").replaceAll("'", "&#039;");
  }

  function escapeAttr(value) {
    return escapeHtml(value).replaceAll("`", "&#096;");
  }

  function controlToast(message, danger = false) {
    let node = document.getElementById("quotaControlToast");
    if (!node) {
      node = document.createElement("div");
      node.id = "quotaControlToast";
      node.className = "quota-control-toast";
      document.body.appendChild(node);
    }
    node.textContent = message;
    node.classList.toggle("danger", danger);
    node.classList.add("visible");
    clearTimeout(controlToast.timer);
    controlToast.timer = setTimeout(() => node.classList.remove("visible"), 3200);
  }

  function scheduleUsageRefresh(delay = 80) {
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(() => loadUsage(true), delay);
  }

  document.addEventListener("click", (event) => {
    const reset = event.target.closest?.("[data-reset-quota]");
    if (reset) {
      resetQuota(reset.dataset.resetQuota, reset);
      return;
    }
    const nav = event.target.closest?.('.nav-button[data-view="accounts"]');
    if (nav) scheduleUsageRefresh(100);
    if (event.target.closest?.("#refreshAccountsButton")) scheduleUsageRefresh(250);
  });

  const observer = new MutationObserver(() => {
    if (!isAccountsViewActive()) return;
    if (accountsContent.querySelector(".account-card") && !accountsContent.querySelector(".usage-overview")) {
      if (lastSummary) renderUsage(lastSummary);
      scheduleUsageRefresh(60);
    }
  });
  observer.observe(accountsContent, { childList: true });

  setInterval(() => {
    if (isAccountsViewActive()) loadUsage();
  }, 15_000);

  if (isAccountsViewActive()) scheduleUsageRefresh();

  globalThis.llmgatewayQuotaUi = { classifyUsage, renderUsage };
})();
