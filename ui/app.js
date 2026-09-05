(() => {
  const THREADS_KEY = "llmgateway.threads.v1"; // v0.3 legacy migration source
  const ACTIVE_THREAD_KEY = "llmgateway.activeThread.v2";
  const LOCAL_KEY = "llmgateway.apiKey.local";
  const SESSION_KEY = "llmgateway.apiKey.session";
  const MIGRATION_KEY = "llmgateway.threads.v1.migrated";

  const state = {
    apiKey: localStorage.getItem(LOCAL_KEY) || sessionStorage.getItem(SESSION_KEY) || "",
    threads: [],
    activeThreadId: localStorage.getItem(ACTIVE_THREAD_KEY),
    models: [],
    catalog: [],
    accounts: [],
    sending: false,
    currentView: "chat",
  };

  const el = (id) => document.getElementById(id);
  const elements = {
    threadList: el("threadList"), newChatButton: el("newChatButton"), threadTitle: el("threadTitle"),
    threadMeta: el("threadMeta"), messages: el("messages"), composerInput: el("composerInput"),
    sendButton: el("sendButton"), modelButton: el("modelButton"), modelButtonText: el("modelButtonText"),
    modelModal: el("modelModal"), modelSearch: el("modelSearch"), modelPickerContent: el("modelPickerContent"),
    authModal: el("authModal"), apiKeyInput: el("apiKeyInput"), rememberKeyInput: el("rememberKeyInput"),
    saveKeyButton: el("saveKeyButton"), authError: el("authError"), statusDot: el("statusDot"),
    statusText: el("statusText"), changeKeyButton: el("changeKeyButton"), routeNotice: el("routeNotice"),
    accountsContent: el("accountsContent"), modelsContent: el("modelsContent"),
    refreshAccountsButton: el("refreshAccountsButton"), modelCatalogSearch: el("modelCatalogSearch"), toast: el("toast"),
  };

  const uid = () => globalThis.crypto?.randomUUID?.() || `${Date.now()}-${Math.random().toString(16).slice(2)}`;
  const activeThread = () => state.threads.find((thread) => thread.id === state.activeThreadId) || null;

  function saveActiveThread() {
    if (state.activeThreadId) localStorage.setItem(ACTIVE_THREAD_KEY, state.activeThreadId);
    else localStorage.removeItem(ACTIVE_THREAD_KEY);
  }

  function draftThread() {
    return {
      id: `draft_${uid()}`,
      title: "New chat",
      model: "llmgateway-auto",
      sticky_route: null,
      messages: [],
      message_count: 0,
      draft: true,
      created_at: new Date().toISOString(),
      updated_at: new Date().toISOString(),
    };
  }

  function createThread() {
    const thread = draftThread();
    state.threads.unshift(thread);
    state.activeThreadId = thread.id;
    saveActiveThread();
    renderThreads();
    renderChat();
    switchView("chat");
    elements.composerInput.focus();
    return thread;
  }

  function ensureThread() {
    if (!state.threads.length) return createThread();
    if (!activeThread()) state.activeThreadId = state.threads[0].id;
    saveActiveThread();
    return activeThread();
  }

  async function loadThreads() {
    if (!state.apiKey) return;
    const response = await apiFetch("/v1/threads");
    if (!response.ok) throw new Error(extractError(await response.text(), response.status));
    const serverThreads = (await response.json()).data || [];

    if (!serverThreads.length) {
      const migrated = await migrateLegacyThreads();
      if (migrated) return loadThreads();
    }

    const existingDrafts = state.threads.filter((thread) => thread.draft);
    state.threads = [...existingDrafts, ...serverThreads.map((thread) => ({ ...thread, messages: null, draft: false }))];
    if (!state.threads.length) createThread();

    if (!state.threads.some((thread) => thread.id === state.activeThreadId)) {
      state.activeThreadId = state.threads[0].id;
    }
    saveActiveThread();
    if (!activeThread()?.draft) await loadThreadDetail(state.activeThreadId);
    renderThreads();
    renderChat();
  }

  async function migrateLegacyThreads() {
    if (localStorage.getItem(MIGRATION_KEY)) return false;
    let legacy = [];
    try { legacy = JSON.parse(localStorage.getItem(THREADS_KEY) || "[]"); } catch (_) { legacy = []; }
    if (!legacy.length) {
      localStorage.setItem(MIGRATION_KEY, "1");
      return false;
    }

    let migrated = 0;
    for (const old of legacy) {
      const messages = (old.messages || [])
        .filter((message) => !message.pending && ["user", "assistant", "system", "tool"].includes(message.role))
        .map((message) => ({ role: message.role, content: message.content ?? "" }));
      const response = await apiFetch("/v1/threads", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          title: old.title || "Imported chat",
          model: old.model || "llmgateway-auto",
          messages,
        }),
      });
      if (response.ok) migrated += 1;
    }
    if (migrated) toast(`Migrated ${migrated} local chat${migrated === 1 ? "" : "s"} to SQLite`);
    localStorage.setItem(MIGRATION_KEY, "1");
    localStorage.removeItem(THREADS_KEY);
    return migrated > 0;
  }

  async function loadThreadDetail(id) {
    if (!id || id.startsWith("draft_")) return activeThread();
    const response = await apiFetch(`/v1/threads/${encodeURIComponent(id)}`);
    if (!response.ok) throw new Error(extractError(await response.text(), response.status));
    const detail = await response.json();
    detail.messages = (detail.messages || []).map(toUiMessage);
    detail.message_count = detail.messages.length;
    detail.draft = false;
    const index = state.threads.findIndex((thread) => thread.id === id);
    if (index >= 0) state.threads[index] = detail;
    else state.threads.unshift(detail);
    return detail;
  }

  function toUiMessage(stored) {
    const message = stored.message || {};
    return {
      id: stored.id || uid(),
      role: stored.role || message.role || "assistant",
      content: messageText(message),
      route: stored.route_id || "",
      createdAt: stored.created_at || Date.now(),
      pending: false,
    };
  }

  function messageText(message) {
    const content = message?.content;
    if (typeof content === "string") return content;
    if (Array.isArray(content)) {
      return content.map((part) => typeof part === "string" ? part : (part?.text || "")).filter(Boolean).join("\n");
    }
    if (content != null) return typeof content === "object" ? JSON.stringify(content, null, 2) : String(content);
    const calls = message?.tool_calls;
    if (Array.isArray(calls) && calls.length) {
      return calls.map((call) => `Tool call: ${call?.function?.name || "tool"}(${call?.function?.arguments || ""})`).join("\n");
    }
    return "";
  }

  async function selectThread(id) {
    state.activeThreadId = id;
    saveActiveThread();
    renderThreads();
    if (!id.startsWith("draft_")) {
      try { await loadThreadDetail(id); } catch (error) { toast(error.message || String(error)); }
    }
    renderChat();
    switchView("chat");
  }

  async function deleteThread(id, event) {
    event?.stopPropagation();
    const thread = state.threads.find((candidate) => candidate.id === id);
    if (!thread) return;
    if (!thread.draft) {
      const response = await apiFetch(`/v1/threads/${encodeURIComponent(id)}`, { method: "DELETE" });
      if (!response.ok) return toast(extractError(await response.text(), response.status));
    }
    state.threads = state.threads.filter((candidate) => candidate.id !== id);
    if (state.activeThreadId === id) state.activeThreadId = state.threads[0]?.id || null;
    if (!state.threads.length) createThread();
    saveActiveThread();
    renderThreads();
    renderChat();
  }

  function renderThreads() {
    elements.threadList.innerHTML = "";
    for (const thread of state.threads) {
      const row = document.createElement("button");
      row.type = "button";
      row.className = `thread-item ${thread.id === state.activeThreadId ? "active" : ""}`;
      row.innerHTML = `<span class="thread-title"></span><button class="thread-delete" title="Delete thread" type="button">×</button>`;
      row.querySelector(".thread-title").textContent = thread.title || "New chat";
      row.addEventListener("click", () => selectThread(thread.id));
      row.querySelector(".thread-delete").addEventListener("click", (event) => deleteThread(thread.id, event));
      elements.threadList.appendChild(row);
    }
  }

  function renderChat() {
    const thread = ensureThread();
    const messages = Array.isArray(thread.messages) ? thread.messages : [];
    elements.threadTitle.textContent = thread.title || "New chat";
    const storage = thread.draft ? "draft" : "SQLite context";
    elements.threadMeta.textContent = `${thread.message_count ?? messages.length} message${(thread.message_count ?? messages.length) === 1 ? "" : "s"} · ${storage}`;
    elements.modelButtonText.textContent = displayModel(thread.model);
    elements.messages.innerHTML = "";

    if (!messages.length) {
      elements.messages.innerHTML = `<div class="empty-state"><div class="empty-state-inner"><h2>One chat. Any route.</h2><p>Your thread context is persisted server-side. Choose a model, and llmgateway keeps the route sticky until failover is needed.</p></div></div>`;
      return;
    }
    for (const message of messages) elements.messages.appendChild(messageNode(message));
    scrollMessages();
  }

  function messageNode(message) {
    const wrapper = document.createElement("article");
    wrapper.className = `message ${message.role}`;
    wrapper.dataset.messageId = message.id;
    const avatar = message.role === "user" ? "YOU" : "AI";
    const role = message.role === "user" ? "You" : "llmgateway";
    wrapper.innerHTML = `<div class="message-avatar">${avatar}</div><div><div class="message-role">${role}</div><div class="message-body"></div><div class="message-route"></div></div>`;
    wrapper.querySelector(".message-body").innerHTML = renderRichText(message.content || "") + (message.pending ? '<span class="typing-cursor"></span>' : "");
    const route = wrapper.querySelector(".message-route");
    if (message.route) route.textContent = `via ${message.route}`; else route.remove();
    return wrapper;
  }

  function renderRichText(text) {
    const parts = String(text).split(/```([\s\S]*?)```/g);
    return parts.map((part, index) => {
      if (index % 2 === 1) {
        let code = part;
        const nl = code.indexOf("\n");
        if (nl > 0 && /^[\w.+#-]+$/.test(code.slice(0, nl).trim())) code = code.slice(nl + 1);
        return `<pre><code>${escapeHtml(code.trimEnd())}</code></pre>`;
      }
      return escapeHtml(part).replace(/`([^`]+)`/g, "<code>$1</code>").replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
        .split(/\n{2,}/).filter(Boolean).map((p) => `<p>${p.replace(/\n/g, "<br>")}</p>`).join("");
    }).join("");
  }

  function escapeHtml(value) {
    return String(value).replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;").replaceAll("'", "&#039;");
  }
  function escapeAttr(value) { return escapeHtml(value).replaceAll("`", "&#096;"); }

  function displayModel(id) {
    if (!id || id === "llmgateway-auto") return "Auto";
    if (id === "llmgateway-coding") return "Coding";
    if (id === "llmgateway-best") return "Best";
    const found = state.models.find((model) => model.id === id);
    return found?.llmgateway?.display_name || id;
  }

  function updateAssistantDom(message) {
    const node = elements.messages.querySelector(`[data-message-id="${CSS.escape(message.id)}"]`);
    if (!node) return renderChat();
    node.querySelector(".message-body").innerHTML = renderRichText(message.content || "") + (message.pending ? '<span class="typing-cursor"></span>' : "");
    const route = node.querySelector(".message-route");
    if (route && message.route) route.textContent = `via ${message.route}`;
    scrollMessages();
  }

  function scrollMessages() { requestAnimationFrame(() => { elements.messages.scrollTop = elements.messages.scrollHeight; }); }
  function autoGrowComposer() { const input = elements.composerInput; input.style.height = "auto"; input.style.height = `${Math.min(input.scrollHeight, 180)}px`; }

  async function materializeDraft(thread, firstContent) {
    if (!thread.draft) return thread;
    const response = await apiFetch("/v1/threads", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ title: makeTitle(firstContent), model: thread.model || "llmgateway-auto" }),
    });
    if (!response.ok) throw new Error(extractError(await response.text(), response.status));
    const created = await response.json();
    created.messages = [];
    created.message_count = 0;
    created.draft = false;
    const index = state.threads.findIndex((candidate) => candidate.id === thread.id);
    if (index >= 0) state.threads[index] = created;
    state.activeThreadId = created.id;
    saveActiveThread();
    return created;
  }

  async function sendMessage() {
    if (state.sending) return;
    const content = elements.composerInput.value.trim();
    if (!content) return;
    if (!state.apiKey) return openAuthModal();

    let thread = ensureThread();
    try { thread = await materializeDraft(thread, content); }
    catch (error) { return toast(error.message || String(error)); }

    if (!Array.isArray(thread.messages)) {
      try { thread = await loadThreadDetail(thread.id); }
      catch (error) { return toast(error.message || String(error)); }
    }

    const userMessage = { id: uid(), role: "user", content, createdAt: Date.now() };
    const assistantMessage = { id: uid(), role: "assistant", content: "", createdAt: Date.now(), pending: true, route: "" };
    thread.messages.push(userMessage, assistantMessage);
    thread.message_count = thread.messages.length;
    elements.composerInput.value = "";
    autoGrowComposer();
    renderThreads();
    renderChat();

    state.sending = true;
    elements.sendButton.disabled = true;
    let succeeded = false;
    try {
      const response = await apiFetch(`/v1/threads/${encodeURIComponent(thread.id)}/messages`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ content, model: thread.model || "llmgateway-auto", stream: true }),
      });
      assistantMessage.route = response.headers.get("x-llmgateway-route") || "";
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      if (!response.body) throw new Error("Gateway returned an empty stream");
      await consumeOpenAiStream(response.body, (delta) => {
        assistantMessage.content += delta;
        updateAssistantDom(assistantMessage);
      });
      assistantMessage.pending = false;
      if (!assistantMessage.content) assistantMessage.content = "The model returned no text content.";
      thread.sticky_route = assistantMessage.route || thread.sticky_route;
      updateAssistantDom(assistantMessage);
      showRoute(assistantMessage.route);
      succeeded = true;
    } catch (error) {
      assistantMessage.pending = false;
      assistantMessage.content = `Request failed: ${error.message || error}`;
      assistantMessage.route = "";
      updateAssistantDom(assistantMessage);
    } finally {
      state.sending = false;
      elements.sendButton.disabled = false;
      elements.composerInput.focus();
      if (succeeded) {
        setTimeout(async () => {
          try { await loadThreadDetail(thread.id); renderThreads(); renderChat(); } catch (_) {}
        }, 80);
      }
    }
  }

  async function consumeOpenAiStream(stream, onText) {
    const reader = stream.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop() || "";
      for (const rawLine of lines) {
        const line = rawLine.trim();
        if (!line.startsWith("data:")) continue;
        const data = line.slice(5).trim();
        if (!data || data === "[DONE]") continue;
        try {
          const event = JSON.parse(data);
          const value = event?.choices?.[0]?.delta?.content;
          if (typeof value === "string") onText(value);
          else if (Array.isArray(value)) for (const part of value) if (typeof part?.text === "string") onText(part.text);
        } catch (_) {}
      }
    }
  }

  function makeTitle(content) { const line = content.replace(/\s+/g, " ").trim(); return line.length > 46 ? `${line.slice(0, 46)}…` : line; }
  function showRoute(route) { if (!route) return; elements.routeNotice.textContent = `✓ Routed through ${route}`; elements.routeNotice.classList.remove("hidden"); clearTimeout(showRoute.timer); showRoute.timer = setTimeout(() => elements.routeNotice.classList.add("hidden"), 4500); }

  async function apiFetch(path, options = {}) {
    const headers = new Headers(options.headers || {});
    if (state.apiKey) headers.set("Authorization", `Bearer ${state.apiKey}`);
    const response = await fetch(path, { ...options, headers });
    if (response.status === 401) openAuthModal("The API key was rejected. Check LLMGATEWAY_API_KEY and try again.");
    return response;
  }

  function extractError(text, status) { try { return JSON.parse(text)?.error?.message || `HTTP ${status}`; } catch (_) { return text || `HTTP ${status}`; } }

  async function loadModels() {
    if (!state.apiKey) return;
    const response = await apiFetch("/v1/models");
    if (!response.ok) throw new Error(extractError(await response.text(), response.status));
    state.models = (await response.json()).data || [];
    renderModelPicker();
    renderChat();
  }

  async function openModelModal() {
    if (!state.apiKey) return openAuthModal();
    elements.modelModal.classList.remove("hidden");
    elements.modelSearch.value = "";
    renderModelPicker();
    try {
      await loadModels();
    } catch (error) {
      toast(`Could not refresh models: ${error.message || String(error)}`);
    }
    requestAnimationFrame(() => elements.modelSearch.focus());
  }

  function renderModelPicker() {
    const query = (elements.modelSearch.value || "").trim().toLowerCase();
    const thread = ensureThread();
    const visible = state.models.filter((model) => {
      const kind = model.llmgateway?.kind;
      if (kind === "route") return false;
      const provider = model.llmgateway?.provider || model.owned_by || "";
      return !query || `${model.id} ${model.owned_by || ""} ${model.llmgateway?.display_name || ""} ${provider} ${displayProvider(provider)}`.toLowerCase().includes(query);
    });
    const virtual = visible.filter((model) => model.llmgateway?.kind === "virtual");
    const groups = new Map();
    for (const model of visible.filter((model) => model.llmgateway?.kind === "physical")) {
      const provider = model.llmgateway?.provider || model.owned_by || "Other";
      if (!groups.has(provider)) groups.set(provider, []);
      groups.get(provider).push(model);
    }
    let html = modelGroupHtml("Smart routing", virtual, thread.model);
    for (const [provider, models] of [...groups.entries()].sort(([a], [b]) => a.localeCompare(b))) {
      html += modelGroupHtml(displayProvider(provider), models, thread.model);
    }
    elements.modelPickerContent.innerHTML = html || '<div class="loading-box">No matching models</div>';
    elements.modelPickerContent.querySelectorAll(".model-choice").forEach((button) => button.addEventListener("click", () => {
      thread.model = button.dataset.modelId;
      elements.modelModal.classList.add("hidden");
      renderChat();
    }));
  }

  function displayProvider(provider) {
    if (provider === "chatgpt-web") return "ChatGPT Web";
    if (provider === "gemini-web") return "Gemini Web";
    if (provider === "qwen-web") return "Qwen Web";
    return provider || "Other";
  }

  function modelGroupHtml(title, models, selectedId) {
    if (!models.length) return "";
    const rows = models.map((model) => {
      const info = model.llmgateway || {};
      const accounts = info.available_accounts != null ? `${info.available_accounts} account${info.available_accounts === 1 ? "" : "s"}` : "routing policy";
      const capabilities = Array.isArray(info.capabilities) && info.capabilities.length ? ` · ${info.capabilities.slice(0, 4).join(", ")}` : "";
      return `<button type="button" class="model-choice ${model.id === selectedId ? "selected" : ""}" data-model-id="${escapeAttr(model.id)}"><span><span class="model-choice-name">${escapeHtml(displayModel(model.id))}</span><span class="model-choice-detail">${escapeHtml(accounts + capabilities)}</span></span><span class="model-choice-check">${model.id === selectedId ? "✓" : ""}</span></button>`;
    }).join("");
    return `<div class="model-group"><div class="model-group-title">${escapeHtml(title)}</div>${rows}</div>`;
  }

  async function loadAccounts(force = false) {
    if (!state.apiKey) return openAuthModal();
    if (!force && state.accounts.length) return renderAccounts();
    elements.accountsContent.innerHTML = '<div class="loading-box">Loading accounts…</div>';
    try {
      const response = await apiFetch("/_llmgateway/accounts");
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      state.accounts = (await response.json()).data || [];
      await Promise.all(state.accounts.map(async (account) => {
        const [modelResponse, transportResponse] = await Promise.all([
          apiFetch(`/_llmgateway/accounts/${encodeURIComponent(account.id)}/models`),
          apiFetch(`/_llmgateway/accounts/${encodeURIComponent(account.id)}/transport`),
        ]);
        account.models = modelResponse.ok ? ((await modelResponse.json()).data || []) : [];
        account.transport_control = transportResponse.ok ? await transportResponse.json() : null;
      }));
      renderAccounts();
    } catch (error) { elements.accountsContent.innerHTML = `<div class="error-box">${escapeHtml(error.message || error)}</div>`; }
  }

  function renderAccounts() {
    if (!state.accounts.length) return void (elements.accountsContent.innerHTML = '<div class="loading-box">No configured accounts</div>');
    elements.accountsContent.innerHTML = `<div class="account-grid">${state.accounts.map((account) => {
      const rows = (account.models || []).map((model) => {
        const binding = model.accounts?.find((candidate) => candidate.account_id === account.id);
        if (!binding) return "";
        // Do not render tombstones left by an older discovery snapshot. A row
        // that is neither configured nor currently discovered is not a model
        // the account can select, even if an older database still contains it.
        if (!binding.configured && !binding.discovered && binding.availability === "unavailable") return "";
        const badges = [binding.availability, ...(model.capabilities || []).slice(0, 3)].map((badge, i) => `<span class="badge ${i === 0 ? escapeAttr(binding.availability) : ""}">${escapeHtml(badge)}</span>`).join("");
        return `<div class="account-model-row"><div><div class="model-name">${escapeHtml(model.display_name || model.external_id)}</div><div class="model-meta">${badges}</div></div><label class="toggle"><input type="checkbox" data-toggle-account="${escapeAttr(account.id)}" data-toggle-model="${escapeAttr(model.id)}" ${binding.enabled ? "checked" : ""}/><span class="toggle-track"></span></label></div>`;
      }).join("") || '<div class="account-model-row"><div class="model-meta">No models discovered yet</div></div>';
      const transport = accountTransportHtml(account);
      return `<article class="account-card"><div class="account-card-header"><div><div class="account-provider">${escapeHtml(account.provider)}</div><div class="account-name">${escapeHtml(account.id)}</div><div class="account-stats">${account.available_model_count} available · ${account.model_count} known</div></div><button type="button" class="secondary-button refresh-account" data-account="${escapeAttr(account.id)}" ${account.discover_models ? "" : "disabled"}>↻ Models</button></div>${transport}<div class="account-models">${rows}</div></article>`;
    }).join("")}</div>`;
    elements.accountsContent.querySelectorAll(".refresh-account").forEach((button) => button.addEventListener("click", () => refreshAccountModels(button.dataset.account, button)));
    elements.accountsContent.querySelectorAll("[data-toggle-model]").forEach((checkbox) => checkbox.addEventListener("change", () => toggleAccountModel(checkbox)));
    elements.accountsContent.querySelectorAll("[data-toggle-browserless]").forEach((checkbox) => checkbox.addEventListener("change", () => toggleBrowserless(checkbox)));
  }

  function accountTransportHtml(account) {
    const transport = account.transport_control;
    if (!transport) return "";
    const capability = transport.browserless || {};
    const browserlessOn = transport.desired_policy === "browserless-preferred";
    const supported = capability.supported === true;
    const effectiveLabels = {
      "direct-http": "Direct HTTP",
      "browser": "Browser",
      "browser-fallback": "Browser fallback",
      "unavailable": "Unavailable",
    };
    const effectiveState = Object.prototype.hasOwnProperty.call(effectiveLabels, transport.effective_transport)
      ? transport.effective_transport
      : "unavailable";
    const effective = effectiveLabels[effectiveState];
    const effectiveTone = effectiveState === "unavailable"
      ? "warning"
      : (effectiveState === "browser-fallback" ? "fallback" : "ready");
    const authWarning = browserlessOn && capability.requires_auth_snapshot && transport.auth_state !== "captured"
      ? '<span class="transport-warning">Authentication required</span>'
      : "";
    const supportNote = supported
      ? (browserlessOn ? "Prefer the transport recommended by this adapter." : "Force browser transport for new requests.")
      : "Browserless is not supported by this adapter.";
    const adapterBadge = transport.effective_adapter_id
      ? `<code class="account-transport-adapter">${escapeHtml(transport.effective_adapter_id)}</code>`
      : "";
    return `
      <section class="account-transport-panel ${browserlessOn ? "is-enabled" : "is-disabled"}">
        <div class="account-transport-row">
          <div class="account-transport-copy">
            <div class="account-transport-label">Browserless</div>
            <div class="account-transport-note">${escapeHtml(supportNote)}</div>
          </div>
          <label class="toggle account-transport-toggle" title="${supported ? "Prefer adapter-recommended browserless transport" : "Browserless is not supported by this adapter"}">
            <input type="checkbox"
              data-toggle-browserless="${escapeAttr(account.id)}"
              ${browserlessOn ? "checked" : ""}
              ${supported ? "" : "disabled"}
              aria-label="Browserless for ${escapeAttr(account.id)}"/>
            <span class="toggle-track"></span>
          </label>
        </div>
        <div class="account-transport-effective ${effectiveTone}">
          <span class="account-transport-status-icon" aria-hidden="true">${effectiveTone === "warning" ? "!" : "✓"}</span>
          <div class="account-transport-status-copy">
            <span class="account-transport-status-label">Effective transport</span>
            <strong>${escapeHtml(effective)}</strong>
            ${authWarning}
          </div>
          ${adapterBadge}
        </div>
      </section>`;
  }

  async function toggleBrowserless(checkbox) {
    const accountId = checkbox.dataset.toggleBrowserless;
    const desired = checkbox.checked;
    checkbox.disabled = true;
    try {
      const response = await apiFetch(`/_llmgateway/accounts/${encodeURIComponent(accountId)}/transport`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ transport_policy: desired ? "browserless-preferred" : "browser-only" }),
      });
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      await response.json();
      toast(`Browserless ${desired ? "enabled" : "disabled"} for ${accountId}`);
      state.accounts = [];
      await loadAccounts(true);
    } catch (error) {
      checkbox.checked = !desired;
      checkbox.disabled = false;
      toast(error.message || String(error));
    }
  }

  async function refreshAccountModels(accountId, button) {
    button.disabled = true; const old = button.textContent; button.textContent = "Refreshing…";
    try {
      const response = await apiFetch(`/_llmgateway/accounts/${encodeURIComponent(accountId)}/models/refresh`, { method: "POST" });
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      const result = await response.json(); toast(`Found ${result.discovered_models} models for ${accountId}`);
      state.accounts = []; state.catalog = []; await loadModels(); await loadAccounts(true);
    } catch (error) { toast(error.message || String(error)); }
    finally { button.disabled = false; button.textContent = old; }
  }

  async function toggleAccountModel(checkbox) {
    const accountId = checkbox.dataset.toggleAccount, modelId = checkbox.dataset.toggleModel;
    checkbox.disabled = true;
    try {
      const response = await apiFetch(`/_llmgateway/accounts/${encodeURIComponent(accountId)}/models`, { method: "PATCH", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ model_id: modelId, enabled: checkbox.checked }) });
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      toast(`${checkbox.checked ? "Enabled" : "Disabled"} ${displayModel(modelId)} on ${accountId}`);
      state.accounts = []; await loadModels(); await loadAccounts(true);
    } catch (error) { checkbox.checked = !checkbox.checked; toast(error.message || String(error)); }
    finally { checkbox.disabled = false; }
  }

  async function loadCatalog(force = false) {
    if (!state.apiKey) return openAuthModal();
    if (!force && state.catalog.length) return renderCatalog();
    elements.modelsContent.innerHTML = '<div class="loading-box">Loading model catalog…</div>';
    try {
      const response = await apiFetch("/_llmgateway/models");
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      state.catalog = (await response.json()).data || []; renderCatalog();
    } catch (error) { elements.modelsContent.innerHTML = `<div class="error-box">${escapeHtml(error.message || error)}</div>`; }
  }

  function renderCatalog() {
    const query = (elements.modelCatalogSearch.value || "").trim().toLowerCase();
    const visible = state.catalog.filter((model) => `${model.id} ${model.display_name} ${model.provider}`.toLowerCase().includes(query));
    const cards = visible.map((model) => {
      const active = (model.accounts || []).filter((a) => a.enabled && ["available", "unknown"].includes(a.availability));
      const badges = (model.capabilities || []).map((c) => `<span class="badge">${escapeHtml(c)}</span>`).join("");
      const context = model.context_window ? `<span class="badge">${Number(model.context_window).toLocaleString()} ctx</span>` : "";
      const accountText = (model.accounts || []).map((a) => `${a.account_id}: ${a.availability}${a.enabled ? "" : " (off)"}`).join(" · ") || "No account bindings";
      return `<article class="catalog-card"><div class="catalog-card-top"><div><div class="catalog-provider">${escapeHtml(model.provider)}</div><div class="catalog-title">${escapeHtml(model.display_name || model.external_id)}</div></div><span class="badge ${active.length ? "available" : "unavailable"}">${active.length} route${active.length === 1 ? "" : "s"}</span></div><div class="catalog-details">${context}${badges}</div><div class="catalog-accounts">${escapeHtml(accountText)}</div></article>`;
    }).join("");
    elements.modelsContent.innerHTML = cards ? `<div class="catalog-grid">${cards}</div>` : '<div class="loading-box">No matching models</div>';
  }

  function switchView(view) {
    state.currentView = view;
    document.querySelectorAll(".view").forEach((node) => node.classList.remove("active-view"));
    document.querySelectorAll(".nav-button").forEach((node) => node.classList.toggle("active", node.dataset.view === view));
    el(`${view}View`)?.classList.add("active-view");
    if (view === "accounts") loadAccounts();
    if (view === "models") loadCatalog();
  }

  async function checkHealth() {
    try {
      const response = await fetch("/_llmgateway/health"); if (!response.ok) throw new Error();
      const payload = await response.json(); elements.statusDot.className = "status-dot ok";
      elements.statusText.textContent = `AI available · ${payload.catalog_models ?? 0} models · ${payload.threads ?? 0} threads`;
    } catch (_) { elements.statusDot.className = "status-dot bad"; elements.statusText.textContent = "Gateway unavailable"; }
  }

  function openAuthModal(message = "") {
    elements.authError.textContent = message; elements.authError.classList.toggle("hidden", !message); elements.authModal.classList.remove("hidden");
    elements.apiKeyInput.value = state.apiKey || ""; elements.rememberKeyInput.checked = Boolean(localStorage.getItem(LOCAL_KEY));
    requestAnimationFrame(() => elements.apiKeyInput.focus());
  }

  async function saveApiKey() {
    const key = elements.apiKeyInput.value.trim();
    if (!key) { elements.authError.textContent = "Enter an API key."; elements.authError.classList.remove("hidden"); return; }
    state.apiKey = key;
    if (elements.rememberKeyInput.checked) { localStorage.setItem(LOCAL_KEY, key); sessionStorage.removeItem(SESSION_KEY); }
    else { sessionStorage.setItem(SESSION_KEY, key); localStorage.removeItem(LOCAL_KEY); }
    elements.saveKeyButton.disabled = true; elements.saveKeyButton.textContent = "Connecting…";
    try {
      await loadModels(); await loadThreads(); elements.authModal.classList.add("hidden"); elements.authError.classList.add("hidden"); toast("Connected to llmgateway");
    } catch (error) { elements.authError.textContent = error.message || String(error); elements.authError.classList.remove("hidden"); }
    finally { elements.saveKeyButton.disabled = false; elements.saveKeyButton.textContent = "Connect"; }
  }

  function changeApiKey() { localStorage.removeItem(LOCAL_KEY); sessionStorage.removeItem(SESSION_KEY); state.apiKey = ""; openAuthModal(); }
  function toast(message) { elements.toast.textContent = message; elements.toast.classList.remove("hidden"); clearTimeout(toast.timer); toast.timer = setTimeout(() => elements.toast.classList.add("hidden"), 3200); }

  function bindEvents() {
    elements.newChatButton.addEventListener("click", createThread);
    elements.modelButton.addEventListener("click", openModelModal);
    elements.sendButton.addEventListener("click", sendMessage);
    elements.composerInput.addEventListener("input", autoGrowComposer);
    elements.composerInput.addEventListener("keydown", (event) => { if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); sendMessage(); } });
    elements.modelSearch.addEventListener("input", renderModelPicker);
    elements.modelCatalogSearch.addEventListener("input", renderCatalog);
    elements.refreshAccountsButton.addEventListener("click", () => loadAccounts(true));
    elements.saveKeyButton.addEventListener("click", saveApiKey);
    elements.apiKeyInput.addEventListener("keydown", (event) => { if (event.key === "Enter") saveApiKey(); });
    elements.changeKeyButton.addEventListener("click", changeApiKey);
    window.addEventListener("llmgateway:models-changed", async () => {
      state.models = [];
      state.catalog = [];
      state.accounts = [];
      try {
        await loadModels();
        if (state.currentView === "accounts") await loadAccounts(true);
        if (state.currentView === "models") await loadCatalog(true);
      } catch (error) {
        toast(`Could not refresh model catalog: ${error.message || String(error)}`);
      }
    });
    document.querySelectorAll(".nav-button").forEach((button) => button.addEventListener("click", () => switchView(button.dataset.view)));
    document.querySelectorAll("[data-close-modal]").forEach((button) => button.addEventListener("click", () => el(button.dataset.closeModal).classList.add("hidden")));
    elements.modelModal.addEventListener("click", (event) => { if (event.target === elements.modelModal) elements.modelModal.classList.add("hidden"); });
    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape") elements.modelModal.classList.add("hidden");
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") { event.preventDefault(); openModelModal(); }
    });
  }

  async function init() {
    bindEvents(); checkHealth(); setInterval(checkHealth, 30_000);
    if (state.apiKey) {
      try { await loadModels(); await loadThreads(); }
      catch (_) { if (!state.threads.length) createThread(); }
    } else { createThread(); openAuthModal(); }
    renderThreads(); renderChat();
  }

  init();
})();
