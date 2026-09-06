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
    accountStatus: "enabled",
    modelStatus: "enabled",
    pendingImages: [],
    pendingFiles: [],
    recentArtifacts: [],
    sending: false,
    recording: false,
    mediaRecorder: null,
    currentAbortController: null,
    voiceMode: "dictation",
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
    imageFileInput: el("imageFileInput"), attachImageButton: el("attachImageButton"),
    documentFileInput: el("documentFileInput"), attachFileButton: el("attachFileButton"),
    micButton: el("micButton"), voiceModeButton: el("voiceModeButton"), voiceStatus: el("voiceStatus"),
    generateImageButton: el("generateImageButton"),
    attachmentPreview: el("attachmentPreview"), composerDropZone: el("composerDropZone"), composerHelp: el("composerHelp"),
    accountsContent: el("accountsContent"), modelsContent: el("modelsContent"),
    accountStatusTabs: el("accountStatusTabs"), modelStatusTabs: el("modelStatusTabs"),
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
      return content.map((part) => {
        if (typeof part === "string") return part;
        if (typeof part?.text === "string" && part.text) return part.text;
        if (["image_url", "input_image", "image"].includes(String(part?.type || ""))) return "📎 Image attachment";
        return "";
      }).filter(Boolean).join("\n");
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
    syncComposerCapabilities();
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
    const body = wrapper.querySelector(".message-body");
    body.innerHTML = renderRichText(message.content || "") + renderImageCards(message) + (message.pending ? '<span class="typing-cursor"></span>' : "");
    body.querySelectorAll("[data-regenerate-image]").forEach((button) => {
      button.addEventListener("click", () => generateImage(message.imagePrompt || ""));
    });
    body.querySelectorAll("[data-edit-image]").forEach((button) => {
      button.addEventListener("click", () => editGeneratedImage(button.dataset.editImage, message.imagePrompt || ""));
    });
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

  function renderImageCards(message) {
    if (!Array.isArray(message?.images) || !message.images.length) return "";
    return `<div class="generated-image-grid">${message.images.map((image) => `
      <figure class="generated-image-card">
        <img src="${escapeAttr(image.url)}" alt="${escapeAttr(message.imagePrompt || "Generated image")}" />
        <figcaption>
          <span>${escapeHtml(image.revised_prompt || "Generated image")}</span>
          <span class="generated-image-actions">
            <button type="button" class="secondary-button compact-action" data-regenerate-image="1">Regenerate</button>
            <button type="button" class="secondary-button compact-action" data-edit-image="${escapeAttr(image.file_id)}">Edit</button>
          </span>
        </figcaption>
      </figure>`).join("")}</div>`;
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

  function selectedModelInputModalities() {
    const modelId = activeThread()?.model || "llmgateway-auto";
    const model = state.models.find((candidate) => candidate.id === modelId);
    const inputs = model?.llmgateway?.multimodal_capabilities?.input_modalities;
    return Array.isArray(inputs) ? inputs : [];
  }

  function selectedModelSupportsImages() {
    const inputs = selectedModelInputModalities();
    return inputs.includes("image");
  }

  function selectedModelSupportsFiles() {
    return selectedModelInputModalities().includes("file");
  }

  function structuredCapabilities(model) {
    return model?.llmgateway?.multimodal_capabilities || {};
  }

  function modelSupportsCapability(model, capability) {
    const capabilities = structuredCapabilities(model);
    if (capability === "image_generation") {
      return capabilities.image_generation === true
        || (Array.isArray(capabilities.output_modalities) && capabilities.output_modalities.includes("image"));
    }
    if (capability === "image_editing") return capabilities.image_editing === true;
    if (capability === "audio_transcription") return capabilities.audio_transcription === true;
    return false;
  }

  function findModelForCapability(capability) {
    const selectedId = activeThread()?.model || "llmgateway-auto";
    const selected = state.models.find((model) => model.id === selectedId);
    if (selected && modelSupportsCapability(selected, capability)) return selected;
    return state.models.find((model) => model.llmgateway?.kind !== "route" && modelSupportsCapability(model, capability)) || null;
  }

  function capabilitySummary(model) {
    const capabilities = structuredCapabilities(model);
    const input = Array.isArray(capabilities.input_modalities) ? capabilities.input_modalities : [];
    const output = Array.isArray(capabilities.output_modalities) ? capabilities.output_modalities : [];
    const parts = [];
    if (input.length) parts.push(`in:${input.join("/")}`);
    if (output.length) parts.push(`out:${output.join("/")}`);
    if (capabilities.audio_transcription) parts.push("STT");
    if (capabilities.image_editing) parts.push("image edit");
    if (capabilities.max_attachment_count) parts.push(`≤${capabilities.max_attachment_count} files`);
    if (capabilities.max_attachment_size_bytes) parts.push(`≤${formatBytes(capabilities.max_attachment_size_bytes)} each`);
    return parts.join(" · ");
  }

  function isNativeDocument(file) {
    const mime = String(file?.type || "").toLowerCase();
    const name = String(file?.name || "").toLowerCase();
    return mime === "application/pdf"
      || mime === "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
      || name.endsWith(".pdf")
      || name.endsWith(".docx");
  }

  function isSupportedDocument(file) {
    const mime = String(file?.type || "").toLowerCase();
    const name = String(file?.name || "").toLowerCase();
    return [
      "application/pdf",
      "text/plain",
      "text/markdown",
      "text/csv",
      "application/json",
      "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ].includes(mime)
      || [".pdf", ".txt", ".md", ".markdown", ".docx", ".csv", ".json"].some((extension) => name.endsWith(extension));
  }

  function syncComposerCapabilities() {
    const supportsImages = selectedModelSupportsImages();
    const supportsFiles = selectedModelSupportsFiles();
    const allPending = [...state.pendingImages, ...state.pendingFiles];
    const busy = allPending.some((item) => item.uploading);
    const failed = allPending.some((item) => item.error);
    const blockedNativeFiles = state.pendingFiles.some((item) => isNativeDocument(item.file) && !supportsFiles);
    if (elements.attachImageButton) {
      elements.attachImageButton.disabled = state.sending || !supportsImages;
      elements.attachImageButton.title = supportsImages
        ? "Attach image"
        : "The selected model has no verified image-input route";
    }
    if (elements.attachFileButton) {
      elements.attachFileButton.disabled = state.sending;
      elements.attachFileButton.title = supportsFiles
        ? "Attach PDF, DOCX, TXT, Markdown, CSV, or JSON"
        : "Attach TXT/Markdown/CSV/JSON; PDF/DOCX require a file-capable model";
    }
    if (elements.composerHelp) {
      if (supportsImages && supportsFiles) {
        elements.composerHelp.textContent = "Enter to send · Shift+Enter for a new line · Paste images or drop attachments";
      } else if (supportsImages) {
        elements.composerHelp.textContent = "Images + text documents available · PDF/DOCX require a file-capable model";
      } else if (supportsFiles) {
        elements.composerHelp.textContent = "Documents available · Image input unavailable for this model";
      } else {
        elements.composerHelp.textContent = "Text documents use safe extraction · Image/PDF/DOCX unavailable for this model";
      }
    }
    if (elements.sendButton && !state.sending) {
      elements.sendButton.disabled = busy
        || failed
        || blockedNativeFiles
        || (state.pendingImages.length > 0 && !supportsImages);
    }
    renderAttachmentPreview();
  }

  function renderAttachmentPreview() {
    if (!elements.attachmentPreview) return;
    const pending = [
      ...state.pendingImages.map((item) => ({ ...item, attachmentKind: "image" })),
      ...state.pendingFiles.map((item) => ({ ...item, attachmentKind: "file" })),
    ];
    if (!pending.length) {
      elements.attachmentPreview.innerHTML = "";
      elements.attachmentPreview.classList.add("hidden");
      return;
    }
    elements.attachmentPreview.classList.remove("hidden");
    elements.attachmentPreview.innerHTML = pending.map((item) => {
      const nativeBlocked = item.attachmentKind === "file"
        && isNativeDocument(item.file)
        && !selectedModelSupportsFiles();
      const stateText = item.error
        ? item.error
        : item.uploading
          ? "Uploading…"
          : nativeBlocked
            ? "Choose a file-capable model"
            : item.attachmentKind === "file" && !isNativeDocument(item.file)
              ? "Ready · text extraction fallback"
              : "Ready";
      const tone = item.error || nativeBlocked ? " error" : "";
      const visual = item.attachmentKind === "image"
        ? `<img src="${escapeAttr(item.previewUrl)}" alt="" />`
        : `<div class="attachment-file-icon" aria-hidden="true">▤</div>`;
      const removeAttr = item.attachmentKind === "image" ? "data-remove-image" : "data-remove-file";
      return `<div class="attachment-chip" data-pending-attachment="${escapeAttr(item.localId)}">
        ${visual}
        <div class="attachment-copy">
          <div class="attachment-name">${escapeHtml(item.file.name)}</div>
          <div class="attachment-state${tone}">${escapeHtml(stateText)}</div>
        </div>
        <button type="button" class="attachment-remove" ${removeAttr}="${escapeAttr(item.localId)}" title="Remove attachment">×</button>
      </div>`;
    }).join("");
    elements.attachmentPreview.querySelectorAll("[data-remove-image]").forEach((button) => {
      button.addEventListener("click", () => removePendingImage(button.dataset.removeImage));
    });
    elements.attachmentPreview.querySelectorAll("[data-remove-file]").forEach((button) => {
      button.addEventListener("click", () => removePendingFile(button.dataset.removeFile));
    });
  }

  async function uploadPendingAttachment(item, purpose) {
    const form = new FormData();
    form.append("purpose", purpose);
    form.append("file", item.file, item.file.name);
    try {
      const response = await apiFetch("/v1/files", { method: "POST", body: form });
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      const uploaded = await response.json();
      item.fileId = uploaded.id;
      item.uploading = false;
      state.recentArtifacts = [
        { fileId: uploaded.id, name: item.file.name, type: item.file.type || uploaded.mime_type || "" },
        ...state.recentArtifacts.filter((artifact) => artifact.fileId !== uploaded.id),
      ].slice(0, 20);
      item.error = "";
    } catch (error) {
      item.uploading = false;
      item.error = error.message || String(error);
    }
    syncComposerCapabilities();
  }

  async function uploadPendingImage(item) {
    return uploadPendingAttachment(item, "vision");
  }

  function addPendingImages(files) {
    if (!selectedModelSupportsImages()) {
      toast("Choose a model with image input before attaching an image.");
      return;
    }
    const images = [...files].filter((file) => String(file.type || "").startsWith("image/"));
    if (!images.length) return;
    const available = Math.max(0, 8 - state.pendingImages.length - state.pendingFiles.length);
    for (const file of images.slice(0, available)) {
      const item = {
        localId: uid(),
        file,
        previewUrl: URL.createObjectURL(file),
        fileId: null,
        uploading: true,
        error: "",
      };
      state.pendingImages.push(item);
      void uploadPendingImage(item);
    }
    if (images.length > available) toast("Up to 8 attachments can be queued at once.");
    syncComposerCapabilities();
  }

  function addPendingFiles(files) {
    const documents = [...files].filter((file) => isSupportedDocument(file) && !String(file.type || "").startsWith("image/"));
    if (!documents.length) return;
    const native = documents.filter(isNativeDocument);
    if (native.length && !selectedModelSupportsFiles()) {
      toast("PDF/DOCX need a model with file input. Text documents can still use extraction fallback.");
    }
    const available = Math.max(0, 8 - state.pendingImages.length - state.pendingFiles.length);
    for (const file of documents.slice(0, available)) {
      const item = {
        localId: uid(),
        file,
        fileId: null,
        uploading: true,
        error: "",
      };
      state.pendingFiles.push(item);
      void uploadPendingAttachment(item, "assistants");
    }
    if (documents.length > available) toast("Up to 8 attachments can be queued at once.");
    syncComposerCapabilities();
  }

  async function removePendingImage(localId) {
    const index = state.pendingImages.findIndex((item) => item.localId === localId);
    if (index < 0) return;
    const [item] = state.pendingImages.splice(index, 1);
    URL.revokeObjectURL(item.previewUrl);
    if (item.fileId) {
      try { await apiFetch(`/v1/files/${encodeURIComponent(item.fileId)}`, { method: "DELETE" }); } catch (_) {}
    }
    syncComposerCapabilities();
  }

  async function removePendingFile(localId) {
    const index = state.pendingFiles.findIndex((item) => item.localId === localId);
    if (index < 0) return;
    const [item] = state.pendingFiles.splice(index, 1);
    if (item.fileId) {
      try { await apiFetch(`/v1/files/${encodeURIComponent(item.fileId)}`, { method: "DELETE" }); } catch (_) {}
    }
    syncComposerCapabilities();
  }

  function clearPendingAttachmentsAfterSend() {
    for (const item of state.pendingImages) URL.revokeObjectURL(item.previewUrl);
    state.pendingImages = [];
    state.pendingFiles = [];
    syncComposerCapabilities();
  }

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
    const attachedImages = state.pendingImages.slice();
    const attachedFiles = state.pendingFiles.slice();
    const attached = [...attachedImages, ...attachedFiles];
    if (!content && !attached.length) return;
    if (!state.apiKey) return openAuthModal();
    if (attached.some((item) => item.uploading)) return toast("Attachment upload is still in progress.");
    if (attached.some((item) => item.error || !item.fileId)) return toast("Remove or retry failed attachment uploads before sending.");
    if (attachedImages.length && !selectedModelSupportsImages()) return toast("The selected model does not support image input.");
    if (attachedFiles.some((item) => isNativeDocument(item.file)) && !selectedModelSupportsFiles()) {
      return toast("The selected model does not support native PDF/DOCX input.");
    }

    let thread = ensureThread();
    const titleSource = content || attached[0]?.file?.name || "Attachment";
    try { thread = await materializeDraft(thread, titleSource); }
    catch (error) { return toast(error.message || String(error)); }

    if (!Array.isArray(thread.messages)) {
      try { thread = await loadThreadDetail(thread.id); }
      catch (error) { return toast(error.message || String(error)); }
    }

    const requestContent = [];
    if (content) requestContent.push({ type: "input_text", text: content });
    for (const item of attachedImages) requestContent.push({ type: "input_image", file_id: item.fileId });
    for (const item of attachedFiles) requestContent.push({ type: "input_file", file_id: item.fileId });
    const userDisplay = [content, ...attached.map((item) => `📎 ${item.file.name}`)].filter(Boolean).join("\n\n");
    const userMessage = { id: uid(), role: "user", content: userDisplay, createdAt: Date.now() };
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
      const controller = new AbortController();
      state.currentAbortController = controller;
      const response = await apiFetch(`/v1/threads/${encodeURIComponent(thread.id)}/messages`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ content: requestContent, model: thread.model || "llmgateway-auto", stream: true }),
        signal: controller.signal,
      });
      assistantMessage.route = response.headers.get("x-llmgateway-route") || "";
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      clearPendingAttachmentsAfterSend();
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
      assistantMessage.content = error?.name === "AbortError"
        ? "Generation stopped."
        : `Request failed: ${error.message || error}`;
      assistantMessage.route = "";
      updateAssistantDom(assistantMessage);
    } finally {
      state.sending = false;
      state.currentAbortController = null;
      elements.sendButton.disabled = false;
      syncComposerCapabilities();
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

  async function generateImage(promptOverride = "") {
    const prompt = String(promptOverride || elements.composerInput.value || "").trim();
    if (!prompt) return toast("Enter an image prompt first.");
    if (!state.apiKey) return openAuthModal();
    const model = findModelForCapability("image_generation");
    if (!model) return toast("No enabled route advertises image generation.");

    let thread = ensureThread();
    try {
      thread = await materializeDraft(thread, prompt);
      if (!Array.isArray(thread.messages)) thread = await loadThreadDetail(thread.id);
    } catch (error) {
      return toast(error.message || String(error));
    }

    const userMessage = { id: uid(), role: "user", content: `🎨 ${prompt}`, createdAt: Date.now() };
    const assistantMessage = { id: uid(), role: "assistant", content: "Generating image…", createdAt: Date.now(), pending: true, images: [], imagePrompt: prompt };
    thread.messages.push(userMessage, assistantMessage);
    thread.message_count = thread.messages.length;
    renderChat();
    state.sending = true;
    syncComposerCapabilities();
    try {
      const response = await apiFetch("/v1/images/generations", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ model: model.id, prompt, n: 1 }),
      });
      assistantMessage.route = response.headers.get("x-llmgateway-route") || "";
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      const payload = await response.json();
      assistantMessage.images = payload.data || [];
      assistantMessage.content = "";
      assistantMessage.pending = false;
      elements.composerInput.value = "";
      autoGrowComposer();
      renderChat();
      showRoute(assistantMessage.route);
    } catch (error) {
      assistantMessage.pending = false;
      assistantMessage.content = `Image generation failed: ${error.message || error}`;
      renderChat();
    } finally {
      state.sending = false;
      syncComposerCapabilities();
    }
  }

  async function editGeneratedImage(fileId, originalPrompt = "") {
    if (!fileId) return;
    const editPrompt = window.prompt("Describe the image edit", originalPrompt ? `Edit: ${originalPrompt}` : "");
    if (!editPrompt?.trim()) return;
    const model = findModelForCapability("image_editing");
    if (!model) return toast("No enabled route advertises image editing.");
    try {
      const source = await apiFetch(`/v1/files/${encodeURIComponent(fileId)}/content`);
      if (!source.ok) throw new Error(extractError(await source.text(), source.status));
      const blob = await source.blob();
      const form = new FormData();
      form.append("model", model.id);
      form.append("prompt", editPrompt.trim());
      form.append("image", blob, "edit-source.png");
      const response = await apiFetch("/v1/images/edits", { method: "POST", body: form });
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      const payload = await response.json();
      const thread = ensureThread();
      if (!Array.isArray(thread.messages)) await loadThreadDetail(thread.id);
      const current = activeThread();
      current.messages.push({
        id: uid(),
        role: "assistant",
        content: "",
        createdAt: Date.now(),
        images: payload.data || [],
        imagePrompt: editPrompt.trim(),
        route: response.headers.get("x-llmgateway-route") || "",
      });
      current.message_count = current.messages.length;
      renderChat();
    } catch (error) {
      toast(`Image edit failed: ${error.message || error}`);
    }
  }

  async function toggleRecording() {
    if (state.recording) {
      state.mediaRecorder?.stop();
      return;
    }
    if (!navigator.mediaDevices?.getUserMedia || typeof MediaRecorder === "undefined") {
      return toast("Microphone recording is not supported by this browser.");
    }
    const model = findModelForCapability("audio_transcription");
    if (!model) return toast("No enabled route advertises audio transcription.");
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      const preferred = ["audio/webm;codecs=opus", "audio/webm", "audio/ogg;codecs=opus"]
        .find((type) => MediaRecorder.isTypeSupported?.(type));
      const recorder = preferred ? new MediaRecorder(stream, { mimeType: preferred }) : new MediaRecorder(stream);
      const chunks = [];
      recorder.addEventListener("dataavailable", (event) => { if (event.data?.size) chunks.push(event.data); });
      recorder.addEventListener("stop", async () => {
        stream.getTracks().forEach((track) => track.stop());
        state.recording = false;
        state.mediaRecorder = null;
        syncVoiceControls();
        const type = recorder.mimeType || chunks[0]?.type || "audio/webm";
        const blob = new Blob(chunks, { type });
        if (!blob.size) return toast("No microphone audio was captured.");
        const form = new FormData();
        form.append("model", model.id);
        form.append("file", blob, type.includes("ogg") ? "voice.ogg" : "voice.webm");
        elements.voiceStatus.textContent = "Transcribing…";
        try {
          const response = await apiFetch("/v1/audio/transcriptions", { method: "POST", body: form });
          if (!response.ok) throw new Error(extractError(await response.text(), response.status));
          const payload = await response.json();
          applyVoiceTranscript(payload.text || "");
        } catch (error) {
          elements.voiceStatus.textContent = "";
          toast(`Transcription failed: ${error.message || error}`);
        }
      });
      state.mediaRecorder = recorder;
      state.recording = true;
      recorder.start();
      syncVoiceControls();
    } catch (error) {
      toast(`Could not start microphone: ${error.message || error}`);
    }
  }

  function syncVoiceControls() {
    if (!elements.micButton) return;
    elements.micButton.classList.toggle("recording", state.recording);
    elements.micButton.textContent = state.recording ? "■" : "🎙";
    elements.micButton.title = state.recording ? "Stop recording" : "Record voice";
    elements.voiceModeButton.textContent = state.voiceMode === "command" ? "Command" : "Dictate";
    if (state.recording) elements.voiceStatus.textContent = state.voiceMode === "command" ? "Listening for a safe command…" : "Listening…";
  }

  function applyVoiceTranscript(transcript) {
    const text = String(transcript || "").trim();
    elements.voiceStatus.textContent = text ? `Heard: ${text}` : "";
    if (!text) return toast("No speech was transcribed.");
    if (state.voiceMode === "dictation") {
      elements.composerInput.value = [elements.composerInput.value.trim(), text].filter(Boolean).join(" ");
      autoGrowComposer();
      elements.composerInput.focus();
      return;
    }
    const command = globalThis.LLMGatewayVoice?.parseCommand?.(text);
    if (!command) return toast("Voice command not recognized. No action was executed.");
    dispatchVoiceCommand(command);
  }

  function dispatchVoiceCommand(command) {
    switch (command.type) {
      case "new_thread":
        createThread();
        toast("Created a new chat.");
        break;
      case "stop_generation":
        if (state.currentAbortController) {
          state.currentAbortController.abort();
          toast("Stopping current generation.");
        } else toast("No active generation to stop.");
        break;
      case "retry": {
        const thread = activeThread();
        const lastUser = [...(thread?.messages || [])].reverse().find((message) => message.role === "user");
        if (!lastUser?.content) return toast("No previous user message to retry.");
        elements.composerInput.value = String(lastUser.content).replace(/^🎨\s*/, "");
        autoGrowComposer();
        sendMessage();
        break;
      }
      case "send_current_draft":
        sendMessage();
        break;
      case "set_model": {
        const query = String(command.query || "").toLowerCase();
        const candidates = state.models.filter((model) => {
          if (model.llmgateway?.kind === "route") return false;
          const haystack = `${model.id} ${model.llmgateway?.display_name || ""}`.toLowerCase();
          return haystack === query || haystack.includes(query);
        });
        if (candidates.length !== 1) return toast(candidates.length ? "Model name is ambiguous." : "No matching model found.");
        ensureThread().model = candidates[0].id;
        renderChat();
        toast(`Selected ${displayModel(candidates[0].id)}.`);
        break;
      }
      case "set_provider": {
        const query = String(command.query || "").toLowerCase();
        const candidates = state.models.filter((model) => {
          if (model.llmgateway?.kind === "route") return false;
          const provider = String(model.llmgateway?.provider || "").toLowerCase();
          return provider === query || provider.includes(query);
        });
        if (candidates.length !== 1) return toast(candidates.length ? "Provider selection is ambiguous; name a model instead." : "No matching provider found.");
        ensureThread().model = candidates[0].id;
        renderChat();
        toast(`Selected ${displayModel(candidates[0].id)}.`);
        break;
      }
      case "attach_artifact": {
        const query = String(command.query || "").toLowerCase();
        const matches = state.recentArtifacts.filter((artifact) => artifact.name.toLowerCase().includes(query));
        if (matches.length !== 1) return toast(matches.length ? "Artifact name is ambiguous." : "No recent matching artifact found.");
        const artifact = matches[0];
        const item = {
          localId: uid(),
          file: { name: artifact.name, type: artifact.type },
          fileId: artifact.fileId,
          uploading: false,
          error: "",
        };
        if (String(artifact.type).startsWith("image/")) {
          item.previewUrl = `/v1/files/${encodeURIComponent(artifact.fileId)}/content`;
          state.pendingImages.push(item);
        } else {
          state.pendingFiles.push(item);
        }
        syncComposerCapabilities();
        toast(`Attached ${artifact.name}.`);
        break;
      }
      default:
        toast("Unsupported voice command. No action was executed.");
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
      syncComposerCapabilities();
    }));
  }

  function displayProvider(provider) {
    const labels = {
      "chatgpt-web": "ChatGPT Web",
      "gemini-web": "Gemini Web",
      "qwen-web": "Qwen Web",
      "deepseek-web": "DeepSeek Web",
      "mimo-web": "Xiaomi MiMo Web",
    };
    return labels[provider] || provider || "Other";
  }

  function modelGroupHtml(title, models, selectedId) {
    if (!models.length) return "";
    const rows = models.map((model) => {
      const info = model.llmgateway || {};
      const accounts = info.available_accounts != null ? `${info.available_accounts} account${info.available_accounts === 1 ? "" : "s"}` : "routing policy";
      const capabilities = Array.isArray(info.capabilities) && info.capabilities.length ? ` · ${info.capabilities.slice(0, 4).join(", ")}` : "";
      const multimodal = capabilitySummary(model);
      const incompatible = (state.pendingImages.length > 0 && !(structuredCapabilities(model).input_modalities || []).includes("image"))
        || (state.pendingFiles.some((item) => isNativeDocument(item.file)) && !(structuredCapabilities(model).input_modalities || []).includes("file"));
      const detail = [accounts + capabilities, multimodal].filter(Boolean).join(" · ");
      return `<button type="button" class="model-choice ${model.id === selectedId ? "selected" : ""} ${incompatible ? "incompatible" : ""}" data-model-id="${escapeAttr(model.id)}" ${incompatible ? "disabled" : ""}><span><span class="model-choice-name">${escapeHtml(displayModel(model.id))}</span><span class="model-choice-detail">${escapeHtml(detail)}</span></span><span class="model-choice-check">${model.id === selectedId ? "✓" : ""}</span></button>`;
    }).join("");
    return `<div class="model-group"><div class="model-group-title">${escapeHtml(title)}</div>${rows}</div>`;
  }

  async function loadAccounts(force = false) {
    if (!state.apiKey) return openAuthModal();
    if (!force && state.accounts.length) return renderAccounts();
    const hasExistingContent = Boolean(elements.accountsContent.querySelector(".account-grid, .account-card"));
    if (!hasExistingContent) {
      elements.accountsContent.innerHTML = '<div class="loading-box">Loading accounts…</div>';
    }
    const savedScrollTop = elements.accountsContent.scrollTop;
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
      renderAccounts(savedScrollTop);
    } catch (error) {
      if (!hasExistingContent) {
        elements.accountsContent.innerHTML = `<div class="error-box">${escapeHtml(error.message || error)}</div>`;
      }
    }
  }

  function updateStatusTabs(container, current, items, enabled) {
    if (!container) return;
    const counts = {
      all: items.length,
      enabled: items.filter(enabled).length,
    };
    counts.disabled = counts.all - counts.enabled;
    container.querySelectorAll(".status-tab").forEach((button) => {
      const status = button.dataset.accountStatus || button.dataset.modelStatus || button.dataset.groupStatus;
      button.classList.toggle("active", status === current);
      const count = button.querySelector("[data-status-count]");
      if (count && Object.prototype.hasOwnProperty.call(counts, status)) {
        count.textContent = String(counts[status]);
      }
    });
  }

  function renderAccounts(restoreScrollTop) {
    const savedScrollTop = typeof restoreScrollTop === "number" ? restoreScrollTop : elements.accountsContent.scrollTop;
    updateStatusTabs(elements.accountStatusTabs, state.accountStatus, state.accounts, (account) => account.enabled === true);
    if (!state.accounts.length) return void (elements.accountsContent.innerHTML = '<div class="loading-box">No configured accounts</div>');

    const visible = state.accounts.filter((account) =>
      state.accountStatus === "all" || (state.accountStatus === "enabled") === (account.enabled === true)
    );
    const existingGrid = elements.accountsContent.querySelector(".account-grid");
    if (!visible.length) {
      if (existingGrid) existingGrid.remove();
      else elements.accountsContent.innerHTML = '<div class="loading-box">No accounts in this status</div>';
      return;
    }

    const gridHtml = `<div class="account-grid">${visible.map((account) => {
      const rows = (account.models || []).map((model) => {
        const binding = model.accounts?.find((candidate) => candidate.account_id === account.id);
        if (!binding) return "";
        const badges = [binding.availability, ...(model.capabilities || []).slice(0, 8)].map((badge, i) => `<span class="badge ${i === 0 ? escapeAttr(binding.availability) : ""}">${escapeHtml(badge)}</span>`).join("");
        return `<div class="account-model-row"><div><div class="model-name">${escapeHtml(model.display_name || model.external_id)}</div><div class="model-meta">${badges}</div></div><label class="toggle" title="Enable this model on ${escapeAttr(account.id)}"><input type="checkbox" data-toggle-account="${escapeAttr(account.id)}" data-toggle-model="${escapeAttr(model.id)}" ${binding.enabled ? "checked" : ""}/><span class="toggle-track"></span></label></div>`;
      }).join("") || '<div class="account-model-row"><div class="model-meta">No models discovered yet</div></div>';
      const transport = accountTransportHtml(account);
      const enabled = account.enabled === true;
      return `<article class="account-card ${enabled ? "" : "is-disabled"}"><div class="account-card-header"><div><div class="account-provider">${escapeHtml(account.provider)}</div><div class="account-name">${escapeHtml(account.id)}</div><div class="account-stats">${account.available_model_count} available · ${account.model_count} known</div></div><div class="entity-state-actions"><span class="badge ${enabled ? "available" : "unavailable"}">${enabled ? "Enabled" : "Disabled"}</span><label class="toggle" title="${enabled ? "Disable account routing" : "Enable account routing"}"><input type="checkbox" data-toggle-account-state="${escapeAttr(account.id)}" ${enabled ? "checked" : ""}/><span class="toggle-track"></span></label><button type="button" class="secondary-button refresh-account" data-account="${escapeAttr(account.id)}" ${account.discover_models ? "" : "disabled"}>↻ Models</button><button type="button" class="secondary-button account-delete" data-delete-account="${escapeAttr(account.id)}">Delete</button></div></div>${transport}<div class="account-models">${rows}</div></article>`;
    }).join("")}</div>`;

    if (existingGrid) {
      existingGrid.outerHTML = gridHtml;
    } else {
      elements.accountsContent.innerHTML = gridHtml;
    }

    elements.accountsContent.querySelectorAll(".refresh-account").forEach((button) => button.addEventListener("click", () => refreshAccountModels(button.dataset.account, button)));
    elements.accountsContent.querySelectorAll("[data-delete-account]").forEach((button) => button.addEventListener("click", () => deleteAccount(button.dataset.deleteAccount, button)));
    elements.accountsContent.querySelectorAll("[data-toggle-account-state]").forEach((checkbox) => checkbox.addEventListener("change", () => toggleAccountState(checkbox)));
    elements.accountsContent.querySelectorAll("[data-toggle-model]").forEach((checkbox) => checkbox.addEventListener("change", () => toggleAccountModel(checkbox)));
    elements.accountsContent.querySelectorAll("[data-toggle-browserless]").forEach((checkbox) => checkbox.addEventListener("change", () => toggleBrowserless(checkbox)));

    if (typeof savedScrollTop === "number") {
      elements.accountsContent.scrollTop = savedScrollTop;
      requestAnimationFrame(() => {
        elements.accountsContent.scrollTop = savedScrollTop;
      });
    }
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

  async function deleteAccount(accountId, button) {
    const confirmed = confirm(
      `Delete account "${accountId}"? This removes its routes and local browser session data. Model-group model entries are preserved and will be ignored if no enabled account can serve them.`
    );
    if (!confirmed) return;

    button.disabled = true;
    const old = button.textContent;
    button.textContent = "Deleting…";
    try {
      const response = await apiFetch(`/_llmgateway/accounts/${encodeURIComponent(accountId)}`, {
        method: "DELETE",
      });
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      state.accounts = [];
      state.catalog = [];
      await loadModels();
      await loadAccounts(true);
      window.dispatchEvent(new CustomEvent("llmgateway:models-changed"));
      toast(`Deleted account ${accountId}`);
    } catch (error) {
      toast(error.message || String(error));
      button.disabled = false;
      button.textContent = old;
    }
  }

  async function toggleAccountState(checkbox) {
    const accountId = checkbox.dataset.toggleAccountState;
    const desired = checkbox.checked;
    checkbox.disabled = true;
    try {
      const response = await apiFetch(`/_llmgateway/accounts/${encodeURIComponent(accountId)}`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled: desired }),
      });
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      toast(`${desired ? "Enabled" : "Disabled"} account ${accountId}`);
      window.dispatchEvent(new CustomEvent("llmgateway:models-changed"));
    } catch (error) {
      checkbox.checked = !desired;
      checkbox.disabled = false;
      toast(error.message || String(error));
    }
  }

  async function toggleAccountModel(checkbox) {
    const accountId = checkbox.dataset.toggleAccount, modelId = checkbox.dataset.toggleModel;
    checkbox.disabled = true;
    try {
      const response = await apiFetch(`/_llmgateway/accounts/${encodeURIComponent(accountId)}/models`, { method: "PATCH", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ model_id: modelId, enabled: checkbox.checked }) });
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      toast(`${checkbox.checked ? "Enabled" : "Disabled"} ${displayModel(modelId)} on ${accountId}`);
      const account = state.accounts.find((a) => a.id === accountId);
      if (account && account.models) {
        const model = account.models.find((m) => m.id === modelId);
        const binding = model?.accounts?.find((b) => b.account_id === accountId);
        if (binding) binding.enabled = checkbox.checked;
      }
      window.dispatchEvent(new CustomEvent("llmgateway:models-changed"));
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
    updateStatusTabs(
      elements.modelStatusTabs,
      state.modelStatus,
      state.catalog,
      (model) => model.fallback_eligible === true
    );
    const visible = state.catalog.filter((model) => {
      const matchesSearch = `${model.id} ${model.display_name} ${model.provider}`.toLowerCase().includes(query);
      const effectiveEnabled = model.fallback_eligible === true;
      const matchesStatus = state.modelStatus === "all" ||
        (state.modelStatus === "enabled") === effectiveEnabled;
      return matchesSearch && matchesStatus;
    });
    const cards = visible.map((model) => {
      const bindings = (model.accounts || []).filter((account) => account.enabled && ["available", "unknown"].includes(account.availability));
      const badges = (model.capabilities || []).map((capability) => `<span class="badge">${escapeHtml(capability)}</span>`).join("");
      const context = model.context_window ? `<span class="badge">${Number(model.context_window).toLocaleString()} ctx</span>` : "";
      const accountText = (model.accounts || []).map((account) => `${account.account_id}: ${account.availability}${account.enabled ? "" : " (model off)"}`).join(" · ") || "No account bindings";
      const globalEnabled = model.enabled === true;
      const effectiveEnabled = model.fallback_eligible === true;
      const statusLabel = effectiveEnabled
        ? "Enabled"
        : (globalEnabled ? "Disabled by account" : "Disabled");
      const fallbackBadge = effectiveEnabled
        ? '<span class="badge available">Fallback ready</span>'
        : '<span class="badge unavailable">Ignored by fallback</span>';
      return `<article class="catalog-card ${effectiveEnabled ? "" : "is-disabled"}"><div class="catalog-card-top"><div><div class="catalog-provider">${escapeHtml(model.provider)}</div><div class="catalog-title">${escapeHtml(model.display_name || model.external_id)}</div></div><div class="entity-state-actions"><span class="badge ${effectiveEnabled ? "available" : "unavailable"}">${escapeHtml(statusLabel)}</span><label class="toggle" title="${globalEnabled ? "Disable model globally" : "Enable model globally"}"><input type="checkbox" data-toggle-catalog-model="${escapeAttr(model.id)}" ${globalEnabled ? "checked" : ""}/><span class="toggle-track"></span></label></div></div><div class="catalog-details">${context}${fallbackBadge}<span class="badge">${bindings.length} enabled binding${bindings.length === 1 ? "" : "s"}</span>${badges}</div><div class="catalog-accounts">${escapeHtml(accountText)}</div></article>`;
    }).join("");
    elements.modelsContent.innerHTML = cards ? `<div class="catalog-grid">${cards}</div>` : '<div class="loading-box">No matching models in this status</div>';
    elements.modelsContent.querySelectorAll("[data-toggle-catalog-model]").forEach((checkbox) =>
      checkbox.addEventListener("change", () => toggleCatalogModel(checkbox))
    );
  }

  async function toggleCatalogModel(checkbox) {
    const modelId = checkbox.dataset.toggleCatalogModel;
    const desired = checkbox.checked;
    checkbox.disabled = true;
    try {
      const response = await apiFetch(`/_llmgateway/models/${encodeURIComponent(modelId)}`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled: desired }),
      });
      if (!response.ok) throw new Error(extractError(await response.text(), response.status));
      toast(`${desired ? "Enabled" : "Disabled"} ${displayModel(modelId)}`);
      window.dispatchEvent(new CustomEvent("llmgateway:models-changed"));
    } catch (error) {
      checkbox.checked = !desired;
      checkbox.disabled = false;
      toast(error.message || String(error));
    }
  }

  function switchView(view) {
    state.currentView = view;
    document.querySelectorAll(".view").forEach((node) => node.classList.remove("active-view"));
    document.querySelectorAll(".nav-button").forEach((node) => node.classList.toggle("active", node.dataset.view === view));
    el(`${view}View`)?.classList.add("active-view");
    if (view === "accounts") loadAccounts();
    if (view === "models") loadCatalog();
    window.dispatchEvent(new CustomEvent("llmgateway:view-changed", { detail: { view } }));
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

  window.addEventListener("llmgateway:accounts-changed", () => {
    state.accounts = [];
    state.catalog = [];
    loadModels().catch((error) => toast(error.message || String(error)));
    if (state.currentView === "accounts") loadAccounts(true);
  });

  function bindEvents() {
    elements.newChatButton.addEventListener("click", createThread);
    elements.modelButton.addEventListener("click", openModelModal);
    elements.sendButton.addEventListener("click", sendMessage);
    elements.attachImageButton.addEventListener("click", () => elements.imageFileInput.click());
    elements.imageFileInput.addEventListener("change", () => {
      addPendingImages(elements.imageFileInput.files || []);
      elements.imageFileInput.value = "";
    });
    elements.attachFileButton.addEventListener("click", () => elements.documentFileInput.click());
    elements.micButton?.addEventListener("click", toggleRecording);
    elements.voiceModeButton?.addEventListener("click", () => {
      state.voiceMode = state.voiceMode === "dictation" ? "command" : "dictation";
      syncVoiceControls();
      toast(state.voiceMode === "command" ? "Voice command mode enabled." : "Voice dictation mode enabled.");
    });
    elements.generateImageButton?.addEventListener("click", () => generateImage());
    elements.documentFileInput.addEventListener("change", () => {
      addPendingFiles(elements.documentFileInput.files || []);
      elements.documentFileInput.value = "";
    });
    elements.composerDropZone.addEventListener("dragover", (event) => {
      event.preventDefault();
      elements.composerDropZone.classList.add("drag-active");
    });
    elements.composerDropZone.addEventListener("dragleave", () => elements.composerDropZone.classList.remove("drag-active"));
    elements.composerDropZone.addEventListener("drop", (event) => {
      event.preventDefault();
      elements.composerDropZone.classList.remove("drag-active");
      const dropped = event.dataTransfer?.files || [];
      addPendingImages(dropped);
      addPendingFiles(dropped);
    });
    elements.composerInput.addEventListener("paste", (event) => {
      const images = [...(event.clipboardData?.files || [])].filter((file) => String(file.type || "").startsWith("image/"));
      if (images.length) addPendingImages(images);
    });
    elements.composerInput.addEventListener("input", autoGrowComposer);
    elements.composerInput.addEventListener("keydown", (event) => { if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); sendMessage(); } });
    elements.modelSearch.addEventListener("input", renderModelPicker);
    elements.modelCatalogSearch.addEventListener("input", renderCatalog);
    elements.accountStatusTabs?.querySelectorAll("[data-account-status]").forEach((button) =>
      button.addEventListener("click", () => {
        state.accountStatus = button.dataset.accountStatus;
        renderAccounts();
      })
    );
    elements.modelStatusTabs?.querySelectorAll("[data-model-status]").forEach((button) =>
      button.addEventListener("click", () => {
        state.modelStatus = button.dataset.modelStatus;
        renderCatalog();
      })
    );
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

  window.llmgatewayUI = { apiFetch, extractError, escapeHtml, escapeAttr, toast };

  async function init() {
    bindEvents(); checkHealth(); setInterval(checkHealth, 30_000);
    if (state.apiKey) {
      try { await loadModels(); await loadThreads(); }
      catch (_) { if (!state.threads.length) createThread(); }
    } else { createThread(); openAuthModal(); }
    renderThreads(); renderChat(); syncVoiceControls();
  }

  init();
})();
