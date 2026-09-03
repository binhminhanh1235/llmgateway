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

      const state = classify(account);
      const session = account.browser_session;
      const credential = account.transport === "api"
        ? account.credential_configured === true ? "Key configured" : "Key missing"
        : session?.label || session?.id || "Browser session";
      const routeCount = (account.route_ids || []).length;

      strip.innerHTML = `
        <span class="account-intel-chip transport-${escapeAttr(account.transport)}">${account.transport === "browser" ? "Browser" : "API"}</span>
        <span class="account-intel-chip state-${escapeAttr(state.level)}"><span class="account-intel-dot"></span>${escapeHtml(state.label)}</span>
        <span class="account-intel-detail">${escapeHtml(credential)}</span>
        <span class="account-intel-detail">${routeCount} route${routeCount === 1 ? "" : "s"}</span>`;

      const message = session?.last_error || state.note;
      strip.title = message || "";
    }
  }

  function classify(account) {
    if (!account.enabled || account.routing_state === "disabled") {
      return { level: "muted", label: "Disabled", note: "This account is disabled" };
    }
    switch (account.routing_state) {
      case "ready":
        return { level: "healthy", label: account.transport === "browser" ? "Connected" : "Ready", note: "Available for routing" };
      case "login_in_progress":
        return { level: "working", label: "Signing in", note: "Waiting for browser login verification" };
      case "requires_login":
        return { level: "warning", label: "Login required", note: "Open the browser session and sign in" };
      case "requires_attention":
        return { level: "danger", label: "Needs attention", note: account.browser_session?.last_error || "Browser session needs attention" };
      case "credential_missing":
        return { level: "danger", label: "Key missing", note: "Configured API credential environment variable is missing" };
      case "unbound":
        return { level: "danger", label: "No session", note: "Browser account has no browser.bindings entry" };
      case "session_unavailable":
      case "browser_runtime_unavailable":
        return { level: "danger", label: "Unavailable", note: "Browser runtime/session is unavailable" };
      default:
        return { level: "warning", label: account.routing_state || "Unknown", note: "Account state is not ready" };
    }
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
