(() => {
  const LOCAL_KEY = "llmgateway.apiKey.local";
  const SESSION_KEY = "llmgateway.apiKey.session";
  const accountsContent = document.getElementById("accountsContent");
  if (!accountsContent) return;

  let loading = false;
  let lastData = [];
  let timer = null;

  function apiKey() {
    return localStorage.getItem(LOCAL_KEY) || sessionStorage.getItem(SESSION_KEY) || "";
  }

  function authHeaders() {
    const key = apiKey();
    return key ? { Authorization: `Bearer ${key}` } : {};
  }

  function active() {
    return document.getElementById("accountsView")?.classList.contains("active-view") === true;
  }

  async function load(force = false) {
    if (!apiKey() || loading || (!force && !active())) return;
    loading = true;
    try {
      const response = await fetch("/_llmgateway/account-intelligence", { headers: authHeaders() });
      if (!response.ok) throw new Error(await response.text() || `HTTP ${response.status}`);
      lastData = (await response.json()).data || [];
      decorate(lastData);
    } catch (error) {
      console.warn("account intelligence unavailable", error);
    } finally {
      loading = false;
    }
  }

  function decorate(accounts) {
    const byId = new Map(accounts.map((account) => [account.id, account]));
    for (const card of accountsContent.querySelectorAll(".account-card")) {
      const accountId = card.querySelector(".account-name")?.textContent?.trim();
      const account = byId.get(accountId);
      if (!account) continue;

      card.dataset.transport = account.transport || "unknown";
      let strip = card.querySelector(".account-intelligence-strip");
      if (!strip) {
        strip = document.createElement("div");
        strip.className = "account-intelligence-strip";
        const stats = card.querySelector(".account-stats");
        if (stats) stats.insertAdjacentElement("afterend", strip);
        else card.querySelector(".account-card-header > div")?.appendChild(strip);
      }

      const effectiveTransport = account.browser_transport?.effective_transport || null;
      const directHttp = effectiveTransport === "direct-http";
      const browserFallback = effectiveTransport === "browser-fallback";
      const transportLabel = directHttp
        ? "Direct HTTP"
        : (browserFallback ? "Browser fallback" : (account.transport === "browser" ? "Browser" : "API"));
      const state = classify(account, effectiveTransport);
      const readiness = account.readiness || {};
      const session = account.browser_session;
      const adapter = account.browser_adapter;
      const credential = account.transport === "api"
        ? account.credential_configured === true ? "Key configured" : "Key missing"
        : session?.label || session?.id || "Browser session";
      const adapterText = account.transport === "browser" && adapter?.adapter_id
        ? adapter.adapter_id + (adapter.adapter_version ? " · " + adapter.adapter_version : "")
        : "";
      const routeCount = Number(readiness.route_count ?? (account.route_ids || []).length);
      const healthyRoutes = Number(readiness.healthy_route_count ?? routeCount);
      const routeText = routeCount ? `${healthyRoutes}/${routeCount} routes healthy` : "dynamic routes";

      strip.innerHTML = `
        <span class="account-intel-chip transport-${escapeAttr(directHttp ? "direct-http" : account.transport)}">${escapeHtml(transportLabel)}</span>
        <span class="account-intel-chip state-${escapeAttr(state.level)}"><span class="account-intel-dot"></span>${escapeHtml(state.label)}</span>
        <span class="account-intel-detail">${escapeHtml(credential)}</span>
        ${adapterText ? `<span class="account-intel-detail">${escapeHtml(adapterText)}</span>` : ""}
        <span class="account-intel-detail">${escapeHtml(routeText)}</span>`;

      const reasons = Array.isArray(readiness.reasons) ? readiness.reasons.join(", ") : "";
      const adapterProblem = adapter && adapter.status !== "ready" ? adapter.message : "";
      const message = adapterProblem || session?.last_error || readiness.browser_adapter_message || state.note || reasons;
      strip.title = message || "";
    }
  }

  function classify(account, effectiveTransport = null) {
    const readiness = account.readiness || {};
    const status = readiness.effective_status || account.routing_state || "unavailable";
    const reasons = new Set(readiness.reasons || []);

    if (reasons.has("account_disabled") || !account.enabled) {
      return { level: "muted", label: "Disabled", note: "This account is disabled" };
    }
    if (status === "ready") {
      return {
        level: "healthy",
        label: effectiveTransport === "direct-http"
          ? "Ready"
          : (account.transport === "browser" ? "Connected" : "Ready"),
        note: "Available for routing",
      };
    }
    if (status === "degraded") {
      if (reasons.has("quota_pressure")) return { level: "warning", label: "Quota pressure", note: "Still routable, but quota pressure is elevated" };
      if (reasons.has("route_cooldown")) return { level: "warning", label: "Degraded", note: "One or more routes are cooling down" };
      return { level: "warning", label: "Degraded", note: [...reasons].join(", ") || "Available with reduced confidence" };
    }

    if (reasons.has("credential_missing")) {
      return { level: "danger", label: "Key missing", note: "Configured API credential environment variable is missing" };
    }
    if (reasons.has("quota_blocked")) {
      return { level: "danger", label: "Quota blocked", note: "Router is excluding this account until quota state recovers" };
    }
    if (reasons.has("browser_adapter_incompatible")) {
      return { level: "danger", label: "Adapter incompatible", note: account.browser_adapter?.message || readiness.browser_adapter_message || "Provider web UI no longer matches the adapter" };
    }
    if (reasons.has("browser_adapter_login_required")) {
      return { level: "warning", label: "Login required", note: account.browser_adapter?.message || "Sign in again in the browser" };
    }
    if (reasons.has("browser_adapter_unavailable")) {
      return { level: "danger", label: "Adapter unavailable", note: account.browser_adapter?.message || "Browser adapter is not available" };
    }
    if (reasons.has("browser_session_stopped")) {
      return { level: "muted", label: "Stopped", note: "Browser session was stopped intentionally" };
    }
    if (reasons.has("browser_login_required")) {
      return { level: "warning", label: "Login required", note: "Open the browser session and sign in" };
    }
    if (reasons.has("browser_session_degraded")) {
      return { level: "warning", label: "Recovering", note: account.browser_session?.last_error || "Browser runtime is being recovered" };
    }
    if (reasons.has("browser_session_requires_attention")) {
      return { level: "danger", label: "Needs attention", note: account.browser_session?.last_error || "Browser authentication needs attention" };
    }
    if (reasons.has("browser_session_failed")) {
      return { level: "danger", label: "Browser failed", note: account.browser_session?.last_error || "Browser recovery failed" };
    }
    if (reasons.has("browser_session_not_ready")) {
      return { level: "danger", label: "Browser unavailable", note: "Browser account is not currently ready for routing" };
    }
    return { level: "danger", label: "Unavailable", note: [...reasons].join(", ") || "Account is not eligible for routing" };
  }

  function escapeHtml(value) {
    return String(value ?? "").replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;").replaceAll("'", "&#039;");
  }

  function escapeAttr(value) {
    return escapeHtml(value).replaceAll("`", "&#096;");
  }

  function schedule(delay = 80) {
    clearTimeout(timer);
    timer = setTimeout(() => load(true), delay);
  }

  document.addEventListener("click", (event) => {
    if (event.target.closest?.('.nav-button[data-view="accounts"]')) schedule(120);
    if (event.target.closest?.("#refreshAccountsButton")) schedule(300);
  });

  const observer = new MutationObserver(() => {
    if (!active() || !accountsContent.querySelector(".account-card")) return;
    if (lastData.length) decorate(lastData);
    schedule(60);
  });
  observer.observe(accountsContent, { childList: true, subtree: false });

  setInterval(() => {
    if (active()) load();
  }, 5_000);

  if (active()) schedule();

  globalThis.llmgatewayAccountIntelligence = { load, classify };
})();
