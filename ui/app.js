(() => {
  const THREADS_KEY = "llmgateway.threads.v1";
  const ACTIVE_THREAD_KEY = "llmgateway.activeThread.v1";
  const LOCAL_KEY = "llmgateway.apiKey.local";
  const SESSION_KEY = "llmgateway.apiKey.session";

  const state = {
    apiKey: localStorage.getItem(LOCAL_KEY) || sessionStorage.getItem(SESSION_KEY) || "",
    threads: loadJson(THREADS_KEY, []),
    activeThreadId: localStorage.getItem(ACTIVE_THREAD_KEY),
    models: [],
    catalog: [],
    accounts: [],
    sending: false,
    currentView: "chat",
  };

  const el = (id) => document.getElementById(id);
  const elements = {
    threadList: el("threadList"),
    newChatButton: el("newChatButton"),
    threadTitle: el("threadTitle"),
    threadMeta: el("threadMeta"),
    messages: el("messages"),
    composerInput: el("composerInput"),
    sendButton: el("sendButton"),
    modelButton: el("modelButton"),
    modelButtonText: el("modelButtonText"),
    modelModal: el("modelModal"),
    modelSearch: el("modelSearch"),
    modelPickerContent: el("modelPickerContent"),
    authModal: el("authModal"),
    apiKeyInput: el("apiKeyInput"),
    rememberKeyInput: el("rememberKeyInput"),
    saveKeyButton: el("saveKeyButton"),
    authError: el("authError"),
    statusDot: el("statusDot"),
    statusText: el("statusText"),
    changeKeyButton: el("changeKeyButton"),
    routeNotice: el("routeNotice"),
    accountsContent: el("accountsContent"),
    modelsContent: el("modelsContent"),
    refreshAccountsButton: el("refreshAccountsButton"),
    modelCatalogSearch: el("modelCatalogSearch"),
    toast: el("toast"),
  };

  function loadJson(key, fallback) {
    try {
      const raw = localStorage.getItem(key);
      return raw ? JSON.parse(raw) : fallback;
    } catch (_) {
      return fallback;
    }
  }

  function saveThreads() {
    localStorage.setItem(THREADS_KEY, JSON.stringify(state.threads));
    if (state.activeThreadId) localStorage.setItem(ACTIVE_THREAD_KEY, state.activeThreadId);
  }

  function uid() {
    return globalThis.crypto?.randomUUID?.() || `${Date.now()}-${Math.random().toString(16).slice(2)}`;
  }

  function activeThread() {
    return state.threads.find((thread) => thread.id === state.activeThreadId) || null;
  }

  function createThread() {
    const thread = {
      id: uid(),
      title: "New chat",
      model: "llmgateway-auto",
      messages: [],
      createdAt: Date.now(),
      updatedAt: Date.now(),
    };
    state.threads.unshift(thread);
    state.activeThreadId = thread.id;
    saveThreads();
    renderThreads();
    renderChat();
    switchView("chat");
    elements.composerInput.focus();
    return thread;
  }

  function ensureThread() {
    if (!state.threads.length) return createThread();
    if (!activeThread()) state.activeThreadId = state.threads[0].id;
    saveThreads();
    return activeThread();
  }

  function deleteThread(id, event) {
    event?.stopPropagation();
    const index = state.threads.findIndex((thread) => thread.id === id);
    if (index < 0) return;
    state.threads.splice(index, 1);
    if (state.activeThreadId === id) {
      state.activeThreadId = state.threads[0]?.id || null;
    }
    saveThreads();
    if (!state.threads.length) createThread();
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
      row.addEventListener("click", () => {
        state.activeThreadId = thread.id;
        saveThreads();
        renderThreads();
        renderChat();
        switchView("chat");
      });
      row.querySelector(".thread-delete").addEventListener("click", (event) => deleteThread(thread.id, event));
      elements.threadList.appendChild(row);
    }
  }

  function renderChat() {
    const thread = ensureThread();
    elements.threadTitle.textContent = thread.title || "New chat";
    elements.threadMeta.textContent = `${thread.messages.length} message${thread.messages.length === 1 ? "" : "s"} · context stays with this thread`;
    elements.modelButtonText.textContent = displayModel(thread.model);
    elements.messages.innerHTML = "";

    if (!thread.messages.length) {
      elements.messages.innerHTML = `
        <div class="empty-state">
          <div class="empty-state-inner">
            <h2>One chat. Any route.</h2>
            <p>Choose Auto or a concrete model. Account switching, failover and quota handling stay behind the curtain.</p>
          </div>
        </div>`;
      return;
    }

    for (const message of thread.messages) {
      elements.messages.appendChild(messageNode(message));
    }
    scrollMessages();
  }

  function messageNode(message) {
    const wrapper = document.createElement("article");
    wrapper.className = `message ${message.role}`;
    wrapper.dataset.messageId = message.id;
    const avatar = message.role === "user" ? "YOU" : "AI";
    const role = message.role === "user" ? "You" : "llmgateway";
    wrapper.innerHTML = `
      <div class="message-avatar">${avatar}</div>
      <div>
        <div class="message-role">${role}</div>
        <div class="message-body"></div>
        <div class="message-route"></div>
      </div>`;
    wrapper.querySelector(".message-body").innerHTML = renderRichText(message.content || "") + (message.pending ? '<span class="typing-cursor"></span>' : "");
    const routeEl = wrapper.querySelector(".message-route");
    if (message.route) routeEl.textContent = `via ${message.route}`;
    else routeEl.remove();
    return wrapper;
  }

  function renderRichText(text) {
    const parts = String(text).split(/```([\s\S]*?)```/g);
    return parts.map((part, index) => {
      if (index % 2 === 1) {
        let code = part;
        const firstNewline = code.indexOf("\n");
        if (firstNewline > 0 && /^[\w.+#-]+$/.test(code.slice(0, firstNewline).trim())) {
          code = code.slice(firstNewline + 1);
        }
        return `<pre><code>${escapeHtml(code.trimEnd())}</code></pre>`;
      }
      const safe = escapeHtml(part)
        .replace(/`([^`]+)`/g, "<code>$1</code>")
        .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
      return safe
        .split(/\n{2,}/)
        .filter(Boolean)
        .map((paragraph) => `<p>${paragraph.replace(/\n/g, "<br>")}</p>`)
        .join("");
    }).join("");
  }

  function escapeHtml(value) {
    return String(value)
      .replaceAll("&", "&amp;")
      .replaceAll("<", "&lt;")
      .replaceAll(">", "&gt;")
      .replaceAll('"', "&quot;")
      .replaceAll("'", "&#039;");
  }

  function displayModel(id) {
    if (!id) return "Auto";
    if (id === "llmgateway-auto") return "Auto";
    if (id === "llmgateway-coding") return "Coding";
    if (id === "llmgateway-best") return "Best";
    const found = state.models.find((model) => model.id === id);
    return found?.llmgateway?.display_name || id;
  }

  function updateAssistantDom(message) {
    const node = elements.messages.querySelector(`[data-message-id="${CSS.escape(message.id)}"]`);
    if (!node) return renderChat();
    const body = node.querySelector(".message-body");
    body.innerHTML = renderRichText(message.content || "") + (message.pending ? '<span class="typing-cursor"></span>' : "");
    const route = node.querySelector(".message-route");
    if (route && message.route) route.textContent = `via ${message.route}`;
    scrollMessages();
  }

  function scrollMessages() {
    requestAnimationFrame(() => {
      elements.messages.scrollTop = elements.messages.scrollHeight;
    });
  }

  function autoGrowComposer() {
    const input = elements.composerInput;
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, 180)}px`;
  }

  async function sendMessage() {
    if (state.sending) return;
    const content = elements.composerInput.value.trim();
    if (!content) return;
    if (!state.apiKey) return openAuthModal();

    const thread = ensureThread();
    const userMessage = { id: uid(), role: "user", content, createdAt: Date.now() };
    thread.messages.push(userMessage);
    if (thread.title === "New chat") {
      thread.title = makeTitle(content);
    }
    thread.updatedAt = Date.now();
    elements.composerInput.value = "";
    autoGrowComposer();

    const requestMessages = thread.messages.map(({ role, content: messageContent }) => ({ role, content: messageContent }));
    const assistantMessage = { id: uid(), role: "assistant", content: "", createdAt: Date.now(), pending: true, route: "" };
    thread.messages.push(assistantMessage);
    saveThreads();
    renderThreads();
    renderChat();

    state.sending = true;
    elements.sendButton.disabled = true;
    try {
      const response = await apiFetch("/v1/chat/completions", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ model: thread.model || "llmgateway-auto", messages: requestMessages, stream: true }),
      });
      assistantMessage.route = response.headers.get("x-llmgateway-route") || "";
      if (!response.ok) {
        const text = await response.text();
        throw new Error(extractError(text, response.status));
      }
      if (!response.body) throw new Error("Gateway returned an empty stream");

      await consumeOpenAiStream(response.body, (delta) => {
        assistantMessage.content += delta;
        updateAssistantDom(assistantMessage);
      });
      assistantMessage.pending = false;
      if (!assistantMessage.content) assistantMessage.content = "The model returned no text content.";
      updateAssistantDom(assistantMessage);
      showRoute(assistantMessage.route);
    } catch (error) {
      assistantMessage.pending = false;
      assistantMessage.content = `Request failed: ${error.message || error}`;
      updateAssistantDom(assistantMessage);
    } finally {
      state.sending = false;
      elements.sendButton.disabled = false;
      thread.updatedAt = Date.now();
      saveThreads();
      renderThreads();
      elements.composerInput.focus();
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
          const content = event?.choices?.[0]?.delta?.content;
          if (typeof content === "string") onText(content);
          else if (Array.isArray(content)) {
            for (const part of content) {
              if (typeof part?.text === "string") onText(part.text);
            }
          }
        } catch (_) {
          // Some upstreams emit auxiliary SSE records. Ignore records that are not OpenAI chunks.
        }
      }
    }
  }

  function makeTitle(content) {
    const oneLine = content.replace(/\s+/g, " ").trim();
    return oneLine.length > 46 ? `${oneLine.slice(0, 46)}…` : oneLine;
  }

  function showRoute(route) {
    if (!route) return;
    elements.routeNotice.textContent = `✓ Routed through ${route}`;
    elements.routeNotice.classList.remove("hidden");
    clearTimeout(showRoute.timer);
    showRoute.timer = setTimeout(() => elements.routeNotice.classList.add("hidden"), 4500);
  }

  async function apiFetch(path, options = {}) {
    const headers = new Headers(options.headers || {});
    if (state.apiKey) headers.set("Authorization", `Bearer ${state.apiKey}`);
    const response = await fetch(path, { ...options, headers });
    if (response.status === 401) {
      openAuthModal("The API key was rejected. Check LLMGATEWAY_API_KEY and try again.");
    }
    return response;
  }

  function extractError(text, status) {
    try {
      const parsed = JSON.parse(text);
      return parsed?.error?.message || `HTTP ${status}`;
    } catch (_) {
      return text || `HTTP ${status}`;
    }
  }

  async function loadModels() {
    if (!state.apiKey) return;
    const response = await apiFetch("/v1/models");
    if (!response.ok) throw new Error(extractError(await response.text(), response.status));
    const payload = await response.json();
    state.models = payload.data || [];
    renderModelPicker();
    renderChat();
  }

  function openModelModal() {
    if (!state.apiKey) return openAuthModal();
    elements.modelModal.classList.remove("hidden");
    elements.modelSearch.value = "";
    renderModelPicker();
    requestAnimationFrame(() => elements.modelSearch.focus());
  }

  function renderModelPicker() {
    const query = (elements.modelSearch.value || "").trim().toLowerCase();
    const thread = ensureThread();
    const visible = state.models.filter((model) => {
      const kind = model.llmgateway?.kind;
      if (kind === "route") return false;
      const haystack = `${model.id} ${model.owned_by || ""} ${model.llmgateway?.display_name || ""} ${model.llmgateway?.provider || ""}`.toLowerCase();
      return !query || haystack.includes(query);
    });
    const virtual = visible.filter((model) => model.llmgateway?.kind === "virtual");
    const physical = visible.filter((model) => model.llmgateway?.kind === "physical");
    const groups = new Map();
    for (const model of physical) {
      const provider = model.llmgateway?.provider || model.owned_by || "Other";
      if (!groups.has(provider)) groups.set(provider, []);
      groups.get(provider).push(model);
    }

    let html = modelGroupHtml("Smart routing", virtual, thread.model);
    for (const [provider, models] of [...groups.entries()].sort(([a], [b]) => a.localeCompare(b))) {
      html += modelGroupHtml(provider, models, thread.model);
    }
    elements.modelPickerContent.innerHTML = html || '<div class="loading-box">No matching models</div>';
    elements.modelPickerContent.querySelectorAll(".model-choice").forEach((button) => {
      button.addEventListener("click", () => {
        thread.model = button.dataset.modelId;
        thread.updatedAt = Date.now();
        saveThreads();
        elements.modelModal.classList.add("hidden");
        renderChat();
      });
    });
  }

  function modelGroupHtml(title, models, selectedId) {
    if (!models.length) return "";
    const rows = models.map((model) => {
      const info = model.llmgateway || {};
      const accounts = info.available_accounts != null ? `${info.available_accounts} account${info.available_accounts === 1 ? "" : "s"}` : "routing policy";
      const capabilities = Array.isArray(info.capabilities) && info.capabilities.length ? ` · ${info.capabilities.slice(0, 4).join(", ")}` : "";
      return `
        <button type="button" class="model-choice ${model.id === selectedId ? "selected" : ""}" data-model-id="${escapeAttr(model.id)}">
          <span>
            <span class="model-choice-name">${escapeHtml(displayModel(model.id))}</span>
            <span class="model-choice-detail">${escapeHtml(accounts + capabilities)}</span>
          </span>
          <span class="model-choice-check">${model.id === selectedId ? "✓" : ""}</span>
        </button>`;
    }).join("");
    return `<div class="model-group"><div class="model-group-title">${escapeHtml(title)}</div>${rows}</div>`;
  }

  function escapeAttr(value) {
    return escapeHtml(value).replaceAll("`", "&#096;");
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
        const modelResponse = await apiFetch(`/_llmgateway/accounts/${encodeURIComponent(account.id)}/models`);
        account.models = modelResponse.ok ? ((await modelResponse.json()).data || []) : [];
      }));
      renderAccounts();
    } catch (error) {
      elements.accountsContent.innerHTML = `<div class="error-box">${escapeHtml(error.message || error)}</div>`;
    }
  }

  function renderAccounts() {
    if (!state.accounts.length) {
      elements.accountsContent.innerHTML = '<div class="loading-box">No configured accounts</div>';
      return;
    }
    const cards = state.accounts.map((account) => {
      const models = account.models || [];
      const modelRows = models.map((model) => {
        const binding = model.accounts?.find((candidate) => candidate.account_id === account.id);
        if (!binding) return "";
        const badges = [binding.availability, ...(model.capabilities || []).slice(0, 3)]
          .map((badge, index) => `<span class="badge ${index === 0 ? escapeAttr(binding.availability) : ""}">${escapeHtml(badge)}</span>`)
          .join("");
        return `
          <div class="account-model-row">
            <div>
              <div class="model-name">${escapeHtml(model.display_name || model.external_id)}</div>
              <div class="model-meta">${badges}</div>
            </div>
            <label class="toggle" title="Allow router to use this model on this account">
              <input type="checkbox" data-toggle-account="${escapeAttr(account.id)}" data-toggle-model="${escapeAttr(model.id)}" ${binding.enabled ? "checked" : ""} />
              <span class="toggle-track"></span>
            </label>
          </div>`;
      }).join("") || '<div class="account-model-row"><div class="model-meta">No models discovered yet</div></div>';
      return `
        <article class="account-card">
          <div class="account-card-header">
            <div>
              <div class="account-provider">${escapeHtml(account.provider)}</div>
              <div class="account-name">${escapeHtml(account.id)}</div>
              <div class="account-stats">${account.available_model_count} available · ${account.model_count} known</div>
            </div>
            <button type="button" class="secondary-button refresh-account" data-account="${escapeAttr(account.id)}" ${account.discover_models ? "" : "disabled"}>↻ Models</button>
          </div>
          <div class="account-models">${modelRows}</div>
        </article>`;
    }).join("");
    elements.accountsContent.innerHTML = `<div class="account-grid">${cards}</div>`;

    elements.accountsContent.querySelectorAll(".refresh-account").forEach((button) => {
      button.addEventListener("click", () => refreshAccountModels(button.dataset.account, button));
    });
    elements.accountsContent.querySelectorAll("[data-toggle-model]").forEach((checkbox) => {
      checkbox.addEventListener("change", () => toggleAccountModel(checkbox));
    });
  }

  async function refreshAccountModels(accountId, button) {
    button.disabled = true;
    const old = button.textContent;
    button.textContent = "Refreshing…";
    try {
      const response = await apiFetch(`/_llmgateway/accounts/${encodeURIComponent(accountId)}/models/refresh`, { method: "POST" });
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      const result = await response.json();
      toast(`Found ${result.discovered_models} models for ${accountId}`);
      state.accounts = [];
      await loadModels();
      await loadAccounts(true);
    } catch (error) {
      toast(error.message || String(error));
    } finally {
      button.disabled = false;
      button.textContent = old;
    }
  }

  async function toggleAccountModel(checkbox) {
    const accountId = checkbox.dataset.toggleAccount;
    const modelId = checkbox.dataset.toggleModel;
    checkbox.disabled = true;
    try {
      const response = await apiFetch(`/_llmgateway/accounts/${encodeURIComponent(accountId)}/models`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ model_id: modelId, enabled: checkbox.checked }),
      });
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      toast(`${checkbox.checked ? "Enabled" : "Disabled"} ${displayModel(modelId)} on ${accountId}`);
      state.accounts = [];
      await loadModels();
      await loadAccounts(true);
    } catch (error) {
      checkbox.checked = !checkbox.checked;
      toast(error.message || String(error));
    } finally {
      checkbox.disabled = false;
    }
  }

  async function loadCatalog(force = false) {
    if (!state.apiKey) return openAuthModal();
    if (!force && state.catalog.length) return renderCatalog();
    elements.modelsContent.innerHTML = '<div class="loading-box">Loading model catalog…</div>';
    try {
      const response = await apiFetch("/_llmgateway/models");
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      state.catalog = (await response.json()).data || [];
      renderCatalog();
    } catch (error) {
      elements.modelsContent.innerHTML = `<div class="error-box">${escapeHtml(error.message || error)}</div>`;
    }
  }

  function renderCatalog() {
    const query = (elements.modelCatalogSearch.value || "").trim().toLowerCase();
    const visible = state.catalog.filter((model) => `${model.id} ${model.display_name} ${model.provider}`.toLowerCase().includes(query));
    const cards = visible.map((model) => {
      const activeAccounts = (model.accounts || []).filter((account) => account.enabled && ["available", "unknown"].includes(account.availability));
      const capabilityBadges = (model.capabilities || []).map((capability) => `<span class="badge">${escapeHtml(capability)}</span>`).join("");
      const context = model.context_window ? `<span class="badge">${Number(model.context_window).toLocaleString()} ctx</span>` : "";
      const accountText = (model.accounts || []).map((account) => `${account.account_id}: ${account.availability}${account.enabled ? "" : " (off)"}`).join(" · ") || "No account bindings";
      return `
        <article class="catalog-card">
          <div class="catalog-card-top">
            <div>
              <div class="catalog-provider">${escapeHtml(model.provider)}</div>
              <div class="catalog-title">${escapeHtml(model.display_name || model.external_id)}</div>
            </div>
            <span class="badge ${activeAccounts.length ? "available" : "unavailable"}">${activeAccounts.length} route${activeAccounts.length === 1 ? "" : "s"}</span>
          </div>
          <div class="catalog-details">${context}${capabilityBadges}</div>
          <div class="catalog-accounts">${escapeHtml(accountText)}</div>
        </article>`;
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
      const response = await fetch("/_llmgateway/health");
      if (!response.ok) throw new Error();
      const payload = await response.json();
      elements.statusDot.className = "status-dot ok";
      elements.statusText.textContent = `AI available · ${payload.catalog_models ?? 0} models`;
    } catch (_) {
      elements.statusDot.className = "status-dot bad";
      elements.statusText.textContent = "Gateway unavailable";
    }
  }

  function openAuthModal(message = "") {
    elements.authError.textContent = message;
    elements.authError.classList.toggle("hidden", !message);
    elements.authModal.classList.remove("hidden");
    elements.apiKeyInput.value = state.apiKey || "";
    elements.rememberKeyInput.checked = Boolean(localStorage.getItem(LOCAL_KEY));
    requestAnimationFrame(() => elements.apiKeyInput.focus());
  }

  async function saveApiKey() {
    const key = elements.apiKeyInput.value.trim();
    if (!key) {
      elements.authError.textContent = "Enter an API key.";
      elements.authError.classList.remove("hidden");
      return;
    }
    state.apiKey = key;
    if (elements.rememberKeyInput.checked) {
      localStorage.setItem(LOCAL_KEY, key);
      sessionStorage.removeItem(SESSION_KEY);
    } else {
      sessionStorage.setItem(SESSION_KEY, key);
      localStorage.removeItem(LOCAL_KEY);
    }
    elements.saveKeyButton.disabled = true;
    elements.saveKeyButton.textContent = "Connecting…";
    try {
      await loadModels();
      elements.authModal.classList.add("hidden");
      elements.authError.classList.add("hidden");
      toast("Connected to llmgateway");
    } catch (error) {
      elements.authError.textContent = error.message || String(error);
      elements.authError.classList.remove("hidden");
    } finally {
      elements.saveKeyButton.disabled = false;
      elements.saveKeyButton.textContent = "Connect";
    }
  }

  function changeApiKey() {
    localStorage.removeItem(LOCAL_KEY);
    sessionStorage.removeItem(SESSION_KEY);
    state.apiKey = "";
    openAuthModal();
  }

  function toast(message) {
    elements.toast.textContent = message;
    elements.toast.classList.remove("hidden");
    clearTimeout(toast.timer);
    toast.timer = setTimeout(() => elements.toast.classList.add("hidden"), 3200);
  }

  function bindEvents() {
    elements.newChatButton.addEventListener("click", createThread);
    elements.modelButton.addEventListener("click", openModelModal);
    elements.sendButton.addEventListener("click", sendMessage);
    elements.composerInput.addEventListener("input", autoGrowComposer);
    elements.composerInput.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && !event.shiftKey) {
        event.preventDefault();
        sendMessage();
      }
    });
    elements.modelSearch.addEventListener("input", renderModelPicker);
    elements.modelCatalogSearch.addEventListener("input", renderCatalog);
    elements.refreshAccountsButton.addEventListener("click", () => loadAccounts(true));
    elements.saveKeyButton.addEventListener("click", saveApiKey);
    elements.apiKeyInput.addEventListener("keydown", (event) => {
      if (event.key === "Enter") saveApiKey();
    });
    elements.changeKeyButton.addEventListener("click", changeApiKey);
    document.querySelectorAll(".nav-button").forEach((button) => button.addEventListener("click", () => switchView(button.dataset.view)));
    document.querySelectorAll("[data-close-modal]").forEach((button) => button.addEventListener("click", () => el(button.dataset.closeModal).classList.add("hidden")));
    elements.modelModal.addEventListener("click", (event) => {
      if (event.target === elements.modelModal) elements.modelModal.classList.add("hidden");
    });
    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape") elements.modelModal.classList.add("hidden");
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        openModelModal();
      }
    });
  }

  async function init() {
    ensureThread();
    bindEvents();
    renderThreads();
    renderChat();
    checkHealth();
    setInterval(checkHealth, 30_000);
    if (state.apiKey) {
      try {
        await loadModels();
      } catch (_) {
        // loadModels opens the auth dialog for 401; other failures can be retried after the gateway starts.
      }
    } else {
      openAuthModal();
    }
  }

  init();
})();
