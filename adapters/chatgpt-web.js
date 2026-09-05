// llmgateway built-in ChatGPT Web adapter.
// Contract v1. Runs inside an authenticated chatgpt.com page through loopback CDP.
// Authentication, CAPTCHA, 2FA, anti-abuse controls, and provider quotas remain interactive/provider-owned.
(() => {
  const CONTRACT_VERSION = 1;
  const ADAPTER_VERSION = "2026.09.05.4";

  const defaults = {
    input: [
      "#prompt-textarea",
      "[data-testid='composer-input']",
      "textarea[data-testid='composer-input']",
      "textarea[name='prompt-textarea']",
      "textarea[placeholder*='Message' i]",
      "textarea[placeholder*='Ask anything' i]",
      "div#prompt-textarea[contenteditable='true']",
      "[contenteditable='true'][data-testid='composer-input']",
      "[contenteditable='true'][role='textbox']"
    ],
    fileInput: [
      "input[type='file'][accept*='image']",
      "input[type='file'][accept*='png']",
      "input[type='file']"
    ],
    send: [
      "#composer-submit-button",
      "button[data-testid='send-button']",
      "button[aria-label='Send prompt']",
      "button[aria-label='Send message']",
      "button[aria-label*='Send' i]"
    ],
    newChat: [
      "a[data-testid='create-new-chat-button']",
      "button[data-testid='create-new-chat-button']",
      "a[aria-label='New chat']",
      "button[aria-label='New chat']",
      "a[href='https://chatgpt.com/']",
      "a[href='/']"
    ],
    response: [
      "[data-message-author-role='assistant'] .markdown",
      "[data-message-author-role='assistant'] .markdown-new-styling",
      "[data-message-author-role='assistant'] .markdown.prose",
      "[data-message-author-role='assistant'] [data-testid='message-content']",
      "[data-message-author-role='assistant'] .prose",
      "article[data-testid^='conversation-turn-'] [data-message-author-role='assistant'] .markdown"
    ],
    completion: [
      "button[data-testid='stop-button']",
      "button[aria-label='Stop generating']",
      "button[aria-label='Stop response']",
      "button[aria-label*='Stop' i]"
    ],
    login: [
      "a[href*='/auth/login']",
      "button[data-testid='login-button']",
      "a[data-testid='login-button']",
      "form[action*='/auth/login']",
      "input[type='password']"
    ],
    modelTrigger: [
      "button.__composer-pill",
      ".__composer-pill",
      "button[data-testid='model-switcher-dropdown-button']",
      "button[aria-label*='model selector' i]",
      "button[aria-label*='model' i]"
    ],
    modelConfigure: [
      "[data-testid='model-configure-modal']",
      "[role='menuitem']"
    ],
    modelModal: [
      "[data-testid='modal-intelligence-menu']"
    ],
    modelOptions: [
      "[data-testid='modal-intelligence-menu'] button[role='radio']",
      "button[role='radio']",
      "[role='menuitem']",
      "[role='option']"
    ]
  };

  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const selectors = (context, key) => {
    const override = context?.selectors?.[key];
    return Array.isArray(override) && override.length ? override : defaults[key] || [];
  };
  const queryFirst = (context, key) => {
    for (const selector of selectors(context, key)) {
      try {
        const node = document.querySelector(selector);
        if (node) return node;
      } catch (_) {}
    }
    return null;
  };
  const queryAll = (context, key) => {
    const out = [];
    for (const selector of selectors(context, key)) {
      try {
        for (const node of document.querySelectorAll(selector)) {
          if (!out.includes(node)) out.push(node);
        }
      } catch (_) {}
    }
    return out;
  };
  const isVisible = (node) => {
    if (!node || node.hidden || node.getAttribute?.("aria-hidden") === "true") return false;
    try {
      const style = globalThis.getComputedStyle?.(node);
      if (style && (style.display === "none" || style.visibility === "hidden")) return false;
    } catch (_) {}
    try {
      const rects = node.getClientRects?.();
      if (rects && rects.length === 0) return false;
    } catch (_) {}
    return true;
  };
  const queryVisible = (context, key) => queryAll(context, key).find(isVisible) || null;
  const loginIndicator = (context) => {
    const direct = queryAll(context, "login").find(isVisible);
    if (direct) return direct;
    try {
      return [...document.querySelectorAll("a,button")]
        .find((node) => isVisible(node) && /^(log in|sign in|sign up)$/i.test(text(node))) || null;
    } catch (_) {
      return null;
    }
  };
  const waitFor = async (fn, timeoutMs = 30000, pollMs = 120) => {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      const value = fn();
      if (value) return value;
      await sleep(pollMs);
    }
    return null;
  };
  const waitForExpectedHost = async (expectedHost, timeoutMs) => {
    const matched = await waitFor(
      () => location.hostname === expectedHost,
      Number(timeoutMs || 8000),
      80
    );
    return Boolean(matched);
  };
  const text = (node) => String(node?.innerText || node?.textContent || "").replace(/\u00a0/g, " ").trim();
  const normalize = (value) => String(value || "").toLowerCase().replace(/[^a-z0-9]+/g, "");

  const contentText = (content) => {
    if (Array.isArray(content)) {
      return content.map((part) => {
        if (typeof part === "string") return part;
        if (part?.type === "text" || part?.type === "input_text") return part.text || "";
        return "";
      }).filter(Boolean).join("\n");
    }
    return String(content || "");
  };

  const imageDataUrls = (request) => {
    const urls = [];
    for (const message of Array.isArray(request?.messages) ? request.messages : []) {
      for (const part of Array.isArray(message?.content) ? message.content : []) {
        if (!["image_url", "input_image", "image"].includes(String(part?.type || ""))) continue;
        const raw = typeof part?.image_url === "string"
          ? part.image_url
          : part?.image_url?.url || part?.url || "";
        if (typeof raw === "string" && raw.startsWith("data:image/")) urls.push(raw);
      }
    }
    return urls;
  };

  const dataUrlFile = (dataUrl, index) => {
    const match = String(dataUrl || "").match(/^data:(image\/[a-z0-9.+-]+);base64,([\s\S]+)$/i);
    if (!match) throw new Error("INVALID_REQUEST: browser image attachment must be a base64 image data URL");
    const mime = match[1].toLowerCase();
    const binary = atob(match[2]);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
    const extension = mime === "image/jpeg" ? "jpg" : (mime.split("/")[1] || "png").replace(/[^a-z0-9]/gi, "");
    return new File([bytes], "llmgateway-image-" + index + "." + extension, { type: mime });
  };

  const attachImages = async (request, context) => {
    const urls = imageDataUrls(request);
    if (!urls.length) return 0;
    const input = await waitFor(() => queryFirst(context, "fileInput"), 8000);
    if (!input) throw new Error("ADAPTER_INCOMPATIBLE: ChatGPT image file input was not found");
    if (urls.length > 1 && input.multiple === false) {
      throw new Error("INVALID_REQUEST: ChatGPT composer currently accepts one image per file input");
    }
    const transfer = new DataTransfer();
    urls.forEach((url, index) => transfer.items.add(dataUrlFile(url, index)));
    try {
      input.files = transfer.files;
    } catch (_) {
      throw new Error("ADAPTER_INCOMPATIBLE: ChatGPT image file input rejected DataTransfer files");
    }
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
    await sleep(350);
    return urls.length;
  };

  const toolProtocol = (request) => {
    const tools = Array.isArray(request?.tools) ? request.tools : [];
    if (!tools.length || request?.tool_choice === "none") return "";
    const definitions = tools.map((tool) => {
      const fn = tool?.function || {};
      return {
        name: String(fn.name || ""),
        description: String(fn.description || ""),
        parameters: fn.parameters || { type: "object" }
      };
    }).filter((tool) => tool.name);

    let choice = request?.tool_choice || "auto";
    if (choice && typeof choice === "object") {
      choice = choice?.function?.name ? "required:" + choice.function.name : "auto";
    }

    return [
      "SYSTEM TOOL PROTOCOL:",
      "You can request llmgateway client tools. Never claim a tool was executed unless you emit a tool call.",
      "Available tools: " + JSON.stringify(definitions),
      "Tool choice: " + String(choice),
      "When a tool is needed, respond ONLY with this exact envelope and no markdown:",
      "[[LLMGATEWAY_TOOL_CALLS]]{\"tool_calls\":[{\"name\":\"tool_name\",\"arguments\":{}}]}[[/LLMGATEWAY_TOOL_CALLS]]",
      "You may return multiple tool_calls. arguments must be a JSON object. If no tool is needed, answer normally."
    ].join("\n");
  };

  const formatMessages = (request) => {
    const messages = Array.isArray(request?.messages) ? request.messages : [];
    const rendered = messages.map((message) => {
      const role = String(message?.role || "user");
      if (role === "assistant" && Array.isArray(message?.tool_calls) && message.tool_calls.length) {
        const calls = message.tool_calls.map((call) => ({
          id: call?.id || "",
          name: call?.function?.name || "",
          arguments: call?.function?.arguments || "{}"
        }));
        return "ASSISTANT_TOOL_CALLS: " + JSON.stringify(calls);
      }
      if (role === "tool") {
        return "TOOL_RESULT"
          + (message?.tool_call_id ? " [" + message.tool_call_id + "]" : "")
          + ": " + contentText(message?.content);
      }
      return role.toUpperCase() + ": " + contentText(message?.content);
    });
    const protocol = toolProtocol(request);
    return (protocol ? [protocol, ...rendered] : rendered).join("\n\n");
  };

  const parseToolCalls = (answer, request) => {
    const tools = Array.isArray(request?.tools) ? request.tools : [];
    if (!tools.length || request?.tool_choice === "none") return null;
    const allowed = new Set(tools.map((tool) => tool?.function?.name).filter(Boolean));
    const rawAnswer = String(answer || "").trim();
    const marked = rawAnswer.match(/\[\[LLMGATEWAY_TOOL_CALLS\]\]\s*([\s\S]*?)\s*\[\[\/LLMGATEWAY_TOOL_CALLS\]\]/i);
    let candidate = marked ? marked[1].trim() : rawAnswer;
    const fenced = candidate.match(/^\`\`\`(?:json)?\s*([\s\S]*?)\s*\`\`\`$/i);
    if (fenced) candidate = fenced[1].trim();
    if (!marked && !candidate.startsWith("{")) return null;
    let payload;
    try {
      payload = JSON.parse(candidate);
    } catch (_) {
      return null;
    }
    const rawCalls = Array.isArray(payload?.tool_calls) ? payload.tool_calls : [];
    const calls = rawCalls.map((call, index) => {
      const name = String(call?.name || "");
      if (!allowed.has(name)) return null;
      let args = call?.arguments;
      if (typeof args === "string") {
        try { args = JSON.parse(args); } catch (_) { args = { _raw: args }; }
      }
      if (!args || typeof args !== "object" || Array.isArray(args)) args = {};
      return {
        index,
        id: "call_browser_" + Date.now() + "_" + index,
        type: "function",
        function: { name, arguments: JSON.stringify(args) }
      };
    }).filter(Boolean);
    return calls.length ? calls : null;
  };

  const openAIResult = (request, model, answer, idPrefix) => {
    const calls = parseToolCalls(answer, request);
    if (request?.stream) {
      const delta = calls
        ? { role: "assistant", tool_calls: calls }
        : { role: "assistant", content: answer };
      return {
        id: idPrefix + Date.now(),
        object: "chat.completion.chunk",
        model,
        choices: [{ index: 0, delta, finish_reason: calls ? "tool_calls" : "stop" }]
      };
    }
    const message = calls
      ? { role: "assistant", content: null, tool_calls: calls }
      : { role: "assistant", content: answer };
    return {
      id: idPrefix + Date.now(),
      object: "chat.completion",
      model,
      choices: [{ index: 0, message, finish_reason: calls ? "tool_calls" : "stop" }]
    };
  };

  const setComposer = async (node, value) => {
    node.focus();
    if (node instanceof HTMLTextAreaElement || node instanceof HTMLInputElement) {
      const proto = node instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, "value")?.set;
      const previous = String(node.value || "");
      if (setter) setter.call(node, value);
      else node.value = value;
      try { node._valueTracker?.setValue?.(previous); } catch (_) {}
      node.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: value }));
      node.dispatchEvent(new Event("change", { bubbles: true }));
      await sleep(0);
      return;
    }

    // ChatGPT's current composer is a ProseMirror contenteditable. Scope the
    // browser-native insertion to the editor itself so ProseMirror observes a
    // real DOM edit instead of mutating the hidden fallback textarea/form.
    let inserted = false;
    try {
      const selection = globalThis.getSelection?.() || document.getSelection?.();
      const range = document.createRange?.();
      if (selection && range) {
        range.selectNodeContents(node);
        selection.removeAllRanges();
        selection.addRange(range);
      }
      if (typeof document.execCommand === "function") {
        inserted = Boolean(document.execCommand("insertText", false, value));
      }
    } catch (_) {}

    if (!inserted || !String(node.textContent || node.innerText || "").includes(value)) {
      // Last-resort DOM mutation for fixture/forward-compatibility. Emit the
      // same input signal ProseMirror/React listen for, then yield so their
      // state can reconcile before submit is attempted.
      node.textContent = value;
      try { node.innerText = value; } catch (_) {}
      node.dispatchEvent(new InputEvent("input", {
        bubbles: true,
        composed: true,
        inputType: "insertText",
        data: value
      }));
      node.dispatchEvent(new Event("change", { bubbles: true }));
    }
    await sleep(0);
    await sleep(0);
  };

  const safeSendControl = (node) => {
    if (!node || !isVisible(node) || node.disabled) return null;
    const label = normalize([
      text(node),
      node.getAttribute?.("aria-label") || "",
      node.getAttribute?.("data-testid") || "",
      node.id || ""
    ].join(" "));
    if (/start voice|voice mode|dictat|microphone/.test(label)) return null;
    return /send|submit|composer-submit-button/.test(label) ? node : null;
  };

  const submitComposer = async (context, composer) => {
    const submitTimeoutMs = Number(context?.submit_timeout_ms || 2500);
    const send = await waitFor(
      () => safeSendControl(queryVisible(context, "send")),
      submitTimeoutMs
    );
    if (send) {
      send.click();
      await sleep(0);
      return "button";
    }
    if (loginIndicator(context)) throw new Error("LOGIN_REQUIRED: ChatGPT session expired");

    // New ChatGPT builds can delay or omit the send button until ProseMirror
    // state catches up. Enter is handled by the editor's React/ProseMirror
    // key path and avoids falling through to the hidden GET fallback form.
    composer.focus();
    for (const type of ["keydown", "keypress", "keyup"]) {
      composer.dispatchEvent(new KeyboardEvent(type, {
        key: "Enter",
        code: "Enter",
        bubbles: true,
        cancelable: true
      }));
    }
    await sleep(120);
    if (String(globalThis.location?.search || "").includes("prompt-textarea=")) {
      throw new Error("ADAPTER_INCOMPATIBLE: ChatGPT composer fell through to the hidden GET fallback form");
    }
    return "enter";
  };

  const selectModel = async (context) => {
    const desired = String(context?.model_label || "").trim();
    if (!desired) return null;
    const desiredNorm = normalize(desired);

    let trigger = queryFirst(context, "modelTrigger");
    if (!trigger) {
      const buttons = [...document.querySelectorAll("button")];
      trigger = buttons.find((button) => /model|gpt|chatgpt|instant|thinking|pro|extended/i.test(text(button))) || null;
    }
    if (!trigger) {
      throw new Error("MODEL_PICKER_NOT_FOUND: ChatGPT model picker is not visible");
    }
    const current = normalize(text(trigger) + " " + (trigger.getAttribute?.("aria-label") || ""));
    if (current.includes(desiredNorm) || desiredNorm.includes(current)) return desired;

    trigger.click();
    await sleep(250);

    const findDesiredOption = () => queryAll(context, "modelOptions").find((node) => {
      if (!isVisible(node)) return false;
      const candidate = normalize(text(node));
      return candidate && (candidate.includes(desiredNorm) || desiredNorm.includes(candidate));
    }) || null;

    let match = findDesiredOption();
    if (!match) {
      const configure = queryAll(context, "modelConfigure").find((node) =>
        isVisible(node) && /configure/i.test(text(node))
      ) || queryFirst(context, "modelConfigure");
      if (configure) {
        configure.click();
        await waitFor(() => queryVisible(context, "modelModal") || findDesiredOption(), 5000);
        match = findDesiredOption();
      }
    }

    if (!match) {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
      throw new Error("MODEL_NOT_FOUND: ChatGPT model not available in picker: " + desired);
    }
    match.click();
    await sleep(300);
    return desired;
  };

  const responseLeaves = (context) => {
    for (const selector of selectors(context, "response")) {
      try {
        const leaves = [...document.querySelectorAll(selector)].filter(isVisible);
        if (leaves.length) return leaves;
      } catch (_) {}
    }
    return [];
  };
  const responseSnapshotKey = (leaves) =>
    leaves.map((node, index) => index + ":" + text(node)).join("\u241e");

  const responseIdentity = (node) => {
    try {
      const turn = node?.closest?.("[data-testid^='conversation-turn-']");
      const turnId = turn?.getAttribute?.("data-testid");
      if (turnId) return "turn:" + turnId;
    } catch (_) {}
    try {
      const message = node?.closest?.("[data-message-id]");
      const messageId = message?.getAttribute?.("data-message-id");
      if (messageId) return "message:" + messageId;
    } catch (_) {}
    try {
      const ownMessageId = node?.getAttribute?.("data-message-id");
      if (ownMessageId) return "message:" + ownMessageId;
    } catch (_) {}
    return "";
  };

  const compactBaselineTexts = (leaves) =>
    leaves
      .slice(-8)
      .map((node) => text(node))
      .filter(Boolean)
      .map((value) => value.slice(0, 4096));

  const compactBaselineKeys = (leaves) =>
    leaves
      .slice(-16)
      .map((node) => responseIdentity(node))
      .filter(Boolean);

  const baselineResult = (leaves) => ({
    count: leaves.length,
    nodes: new Set(leaves),
    texts: compactBaselineTexts(leaves),
    keys: compactBaselineKeys(leaves)
  });

  const matchesBaselineText = (value, baselineTexts) => {
    const candidate = String(value || "");
    if (!candidate) return false;
    return (baselineTexts || []).some((baseline) => {
      const prior = String(baseline || "");
      if (!prior) return false;
      return candidate === prior || candidate.startsWith(prior) || prior.startsWith(candidate);
    });
  };

  const recoveredResponseText = (state, leaves) => {
    if (!leaves.length) return "";
    const baselineKeys = new Set(Array.isArray(state?.baselineKeys) ? state.baselineKeys : []);
    const baselineTexts = Array.isArray(state?.baselineTexts) ? state.baselineTexts : [];
    const baselineCount = Number(state?.baselineCount || 0);

    for (let index = leaves.length - 1; index >= 0; index -= 1) {
      const node = leaves[index];
      const value = text(node);
      if (!value) continue;

      const identity = responseIdentity(node);
      if (identity && !baselineKeys.has(identity)) return value;

      // Normal non-virtualized DOM: a new assistant node appears after the
      // pre-submit baseline.
      if (index >= baselineCount) return value;

      // ChatGPT may replace the document and hydrate only a window of the
      // conversation. In that case the new response can be the sole visible
      // assistant node, so absolute node count is no longer meaningful.
      // Compare against a compact pre-submit text snapshot instead.
      if (!matchesBaselineText(value, baselineTexts)) return value;
    }

    return "";
  };

  const captureResponseBaseline = async (context) => {
    if (!context?.reuse_native_conversation) {
      const leaves = responseLeaves(context);
      return baselineResult(leaves);
    }

    const timeoutMs = Number(context?.history_hydration_timeout_ms || 12000);
    const stableMs = Number(context?.history_stable_ms || 1200);
    const deadline = Date.now() + timeoutMs;
    let lastKey = null;
    let stableSince = 0;
    let lastLeaves = [];

    while (Date.now() < deadline) {
      const leaves = responseLeaves(context);
      const key = responseSnapshotKey(leaves);
      if (leaves.length && key === lastKey) {
        if (stableSince && Date.now() - stableSince >= stableMs) {
          return baselineResult(leaves);
        }
      } else {
        lastKey = key;
        lastLeaves = leaves;
        stableSince = leaves.length ? Date.now() : 0;
      }
      await sleep(120);
    }

    if (!lastLeaves.length) {
      throw new Error("ADAPTER_INCOMPATIBLE: ChatGPT conversation history did not load");
    }
    throw new Error("ADAPTER_INCOMPATIBLE: ChatGPT conversation history did not stabilize");
  };

  const newResponseText = (baseline, leaves) => {
    if (!baseline || leaves.length <= baseline.count) return "";
    for (let index = leaves.length - 1; index >= baseline.count; index -= 1) {
      const node = leaves[index];
      if (!baseline.nodes.has(node)) {
        const value = text(node);
        if (value) return value;
      }
    }
    return "";
  };

  const freshChatLocation = () => {
    const path = String(location.pathname || "/").replace(/\/+$/, "") || "/";
    return location.hostname === "chatgpt.com" && path === "/";
  };

  const startNewConversation = async (context) => {
    if (!context?.start_new_conversation) return;
    if (freshChatLocation() && responseLeaves(context).length === 0) return;
    const newChat = await waitFor(() => queryVisible(context, "newChat"), 8000);
    if (!newChat) {
      throw new Error("ADAPTER_INCOMPATIBLE: ChatGPT New chat control was not found");
    }
    newChat.click();
    const reset = await waitFor(
      () => freshChatLocation() && responseLeaves(context).length === 0,
      8000
    );
    if (!reset) {
      throw new Error("ADAPTER_INCOMPATIBLE: ChatGPT did not open a clean fresh chat");
    }
    await sleep(250);
  };

  const streamJobs = globalThis.__LLMGATEWAY_STREAM_JOBS__ || new Map();
  globalThis.__LLMGATEWAY_STREAM_JOBS__ = streamJobs;

  const STREAM_RECOVERY_PREFIX = "__llmgateway_chatgpt_stream__:";
  const recoveryStorage = () => {
    try { return globalThis.sessionStorage || null; } catch (_) { return null; }
  };
  const recoveryKey = (streamId) => STREAM_RECOVERY_PREFIX + String(streamId || "");
  const clearStreamRecovery = (streamId) => {
    const storage = recoveryStorage();
    if (!storage) return;
    try { storage.removeItem(recoveryKey(streamId)); } catch (_) {}
  };
  const saveStreamRecovery = (state) => {
    if (!state?.recoveryArmed) return;
    const storage = recoveryStorage();
    if (!storage) return;
    const request = state.request || {};
    const context = state.context || {};
    const payload = {
      streamId: state.streamId,
      completionId: state.completionId,
      model: state.model,
      baselineCount: Number(state.baselineCount || 0),
      baselineTexts: Array.isArray(state.baselineTexts) ? state.baselineTexts : [],
      baselineKeys: Array.isArray(state.baselineKeys) ? state.baselineKeys : [],
      selectors: context.selectors && typeof context.selectors === "object" ? context.selectors : null,
      delivered: String(state.delivered || ""),
      roleEmitted: Boolean(state.roleEmitted),
      progressSeq: Number(state.progressSeq || 0),
      progressPhase: String(state.progressPhase || "submitted"),
      lastProgressAt: Number(state.lastProgressAt || Date.now()),
      startedAt: Number(state.startedAt || Date.now()),
      stableSince: Number(state.stableSince || 0),
      responseTimeoutMs: Number(context.response_timeout_ms || 180000),
      responseStableMs: Number(context.response_stable_ms || 900),
      tools: Array.isArray(request.tools) ? request.tools : [],
      toolChoice: request.tool_choice ?? "auto"
    };
    try { storage.setItem(recoveryKey(state.streamId), JSON.stringify(payload)); } catch (_) {}
  };
  const recoverStreamJob = (streamId) => {
    const storage = recoveryStorage();
    if (!storage) return null;
    let payload;
    try {
      payload = JSON.parse(storage.getItem(recoveryKey(streamId)) || "null");
    } catch (_) {
      clearStreamRecovery(streamId);
      return null;
    }
    if (!payload || payload.streamId !== streamId) return null;
    const startedAt = Number(payload.startedAt || 0);
    const responseTimeoutMs = Number(payload.responseTimeoutMs || 180000);
    if (!startedAt || Date.now() - startedAt > Math.max(responseTimeoutMs + 60000, 300000)) {
      clearStreamRecovery(streamId);
      return null;
    }
    const state = {
      streamId,
      completionId: String(payload.completionId || ("chatcmpl_chatgpt_web_" + Date.now())),
      model: String(payload.model || "chatgpt-web"),
      request: {
        stream: true,
        tools: Array.isArray(payload.tools) ? payload.tools : [],
        tool_choice: payload.toolChoice ?? "auto"
      },
      context: {
        response_timeout_ms: responseTimeoutMs,
        response_stable_ms: Number(payload.responseStableMs || 900),
        selectors: payload.selectors && typeof payload.selectors === "object" ? payload.selectors : undefined
      },
      answer: "",
      delivered: String(payload.delivered || ""),
      done: false,
      finalEmitted: false,
      roleEmitted: Boolean(payload.roleEmitted),
      cancelled: false,
      error: null,
      toolCalls: null,
      progressSeq: Number(payload.progressSeq || 1),
      progressPhase: String(payload.progressPhase || "recovered"),
      lastProgressAt: Number(payload.lastProgressAt || Date.now()),
      startedAt,
      stableSince: Number(payload.stableSince || 0),
      baselineCount: Number(payload.baselineCount || 0),
      baselineTexts: Array.isArray(payload.baselineTexts) ? payload.baselineTexts : [],
      baselineKeys: Array.isArray(payload.baselineKeys) ? payload.baselineKeys : [],
      recoveryArmed: true,
      recovered: true
    };
    streamJobs.set(streamId, state);
    return state;
  };

  const classifyStreamError = (error) => {
    const message = String(error?.message || error || "browser stream failed");
    let code = "adapter_execution_error";
    if (/^ADAPTER_INCOMPATIBLE:/i.test(message)) code = "adapter_incompatible";
    else if (/^(MODEL_NOT_FOUND|MODEL_PICKER_NOT_FOUND):/i.test(message)) code = "model_unavailable";
    else if (/^LOGIN_REQUIRED:/i.test(message)) code = "login_required";
    else if (/^RESPONSE_TIMEOUT:/i.test(message)) code = "response_timeout";
    else if (/^STREAM_CANCELLED:/i.test(message)) code = "cancelled";
    else if (/^INVALID_REQUEST:/i.test(message)) code = "invalid_request";
    return { code, message };
  };

  const STREAM_REWRITE_GUARD_CHARS = 128;

  const guardedStreamPrefix = (value, done = false) => {
    const text = String(value || "");
    if (done) return text;
    const chars = Array.from(text);
    if (chars.length <= STREAM_REWRITE_GUARD_CHARS) return "";
    return chars.slice(0, chars.length - STREAM_REWRITE_GUARD_CHARS).join("");
  };

  const streamChunk = (state, delta, finishReason = null) => ({
    id: state.completionId,
    object: "chat.completion.chunk",
    model: state.model,
    choices: [{ index: 0, delta, finish_reason: finishReason }]
  });

  const markStreamProgress = (state, phase, heartbeat = false) => {
    const nextPhase = String(phase || "active");
    const now = Date.now();
    const phaseChanged = state.progressPhase !== nextPhase;
    const heartbeatDue = heartbeat && now - Number(state.lastProgressAt || 0) >= 1000;
    if (!phaseChanged && !heartbeatDue) return;
    state.progressPhase = nextPhase;
    state.progressSeq = Number(state.progressSeq || 0) + 1;
    state.lastProgressAt = now;
    saveStreamRecovery(state);
  };

  const startStreamJob = async (request, context) => {
    const streamId = "chatgpt_stream_" + Date.now() + "_" + Math.random().toString(36).slice(2);
    const state = {
      streamId,
      completionId: "chatcmpl_chatgpt_web_" + Date.now(),
      model: request?.model || context?.model_label || "chatgpt-web",
      request,
      context,
      answer: "",
      delivered: "",
      done: false,
      finalEmitted: false,
      roleEmitted: false,
      cancelled: false,
      error: null,
      toolCalls: null,
      progressSeq: 1,
      progressPhase: "starting",
      lastProgressAt: Date.now(),
      startedAt: Date.now(),
      stableSince: 0,
      baselineCount: 0,
      baselineTexts: [],
      baselineKeys: [],
      recoveryArmed: false,
      recovered: false
    };
    streamJobs.set(streamId, state);

    let before;
    try {
      await selectModel(context);
      markStreamProgress(state, "model-ready");
      await startNewConversation(context);
      markStreamProgress(state, "conversation-ready");
      const composer = await waitFor(() => queryVisible(context, "input"), 15000);
      if (!composer) throw new Error("ADAPTER_INCOMPATIBLE: ChatGPT prompt composer disappeared");
      markStreamProgress(state, "composer-ready");

      before = await captureResponseBaseline(context);
      state.baselineCount = Number(before?.count || 0);
      state.baselineTexts = Array.isArray(before?.texts) ? before.texts : [];
      state.baselineKeys = Array.isArray(before?.keys) ? before.keys : [];
      markStreamProgress(state, "history-ready");
      const prompt = formatMessages(request);
      if (!prompt.trim()) throw new Error("INVALID_REQUEST: no textual messages to submit");
      await setComposer(composer, prompt);
      markStreamProgress(state, "prompt-ready");
      const attachedImages = await attachImages(request, context);
      if (attachedImages) markStreamProgress(state, "attachments-ready");

      state.recoveryArmed = true;
      saveStreamRecovery(state);
      await submitComposer(context, composer);
      markStreamProgress(state, "submitted");
    } catch (error) {
      state.error = classifyStreamError(error);
      state.done = true;
      markStreamProgress(state, "failed");
      return {
        stream_id: streamId,
        status: 200,
        content_type: "text/event-stream"
      };
    }

    Promise.resolve().then(async () => {
      try {
        let last = "";
        let answer = "";
        while (Date.now() - state.startedAt < Number(context?.response_timeout_ms || 180000)) {
          if (state.cancelled) {
            const stop = queryVisible(context, "completion");
            try { stop?.click(); } catch (_) {}
            throw new Error("STREAM_CANCELLED: client disconnected or cancelled");
          }
          if (loginIndicator(context)) {
            throw new Error("LOGIN_REQUIRED: ChatGPT session expired while waiting for a response");
          }
          const responses = responseLeaves(context);
          const candidate = newResponseText(before, responses);
          const responseAdvanced = Boolean(candidate);
          const generating = Boolean(queryVisible(context, "completion"));
          const submittedAndWaiting = generating || !queryVisible(context, "send");
          if (submittedAndWaiting) {
            markStreamProgress(state, generating ? "generating" : "submitted-wait", true);
          }
          if (!responseAdvanced) {
            await sleep(120);
            continue;
          }
          if (candidate && candidate !== last) {
            last = candidate;
            answer = candidate;
            state.answer = candidate;
            state.stableSince = Date.now();
            markStreamProgress(state, "response-advanced", true);
          } else if (candidate && !generating && state.stableSince && Date.now() - state.stableSince >= Number(context?.response_stable_ms || 900)) {
            answer = candidate;
            state.answer = candidate;
            break;
          }
          await sleep(120);
        }
        if (!answer) throw new Error("RESPONSE_TIMEOUT: ChatGPT did not produce a readable response");

        state.answer = answer;
        state.toolCalls = parseToolCalls(answer, request);
        state.done = true;
        markStreamProgress(state, "completed");
      } catch (error) {
        state.error = classifyStreamError(error);
        state.done = true;
        markStreamProgress(state, "failed");
      } finally {
        const cleanupTimer = setTimeout(() => {
          if (streamJobs.get(streamId) === state) streamJobs.delete(streamId);
          clearStreamRecovery(streamId);
        }, 60000);
        cleanupTimer?.unref?.();
      }
    });

    return {
      stream_id: streamId,
      status: 200,
      content_type: "text/event-stream"
    };
  };

  const advanceRecoveredStream = (state) => {
    if (!state?.recovered || state.done || state.error) return;
    try {
      if (state.cancelled) throw new Error("STREAM_CANCELLED: client disconnected or cancelled");
      if (loginIndicator(state.context)) {
        throw new Error("LOGIN_REQUIRED: ChatGPT session expired while recovering a response");
      }

      const now = Date.now();
      const responses = responseLeaves(state.context);
      const candidate = recoveredResponseText(state, responses);
      const generating = Boolean(queryVisible(state.context, "completion"));
      const submittedAndWaiting = generating || !queryVisible(state.context, "send");
      if (submittedAndWaiting) {
        markStreamProgress(state, generating ? "generating" : "submitted-wait", true);
      }

      if (candidate && candidate !== state.answer) {
        state.answer = candidate;
        state.stableSince = now;
        markStreamProgress(state, "response-advanced", true);
      } else if (
        candidate &&
        !generating &&
        state.stableSince &&
        now - state.stableSince >= Number(state.context?.response_stable_ms || 900)
      ) {
        state.answer = candidate;
        state.toolCalls = parseToolCalls(candidate, state.request);
        state.done = true;
        markStreamProgress(state, "completed");
      } else if (now - state.startedAt >= Number(state.context?.response_timeout_ms || 180000)) {
        throw new Error("RESPONSE_TIMEOUT: ChatGPT did not produce a readable response after browser navigation recovery");
      }

      saveStreamRecovery(state);
    } catch (error) {
      state.error = classifyStreamError(error);
      state.done = true;
      markStreamProgress(state, "failed");
    }
  };

  const pollStreamJob = (streamId) => {
    const normalizedStreamId = String(streamId || "");
    const state = streamJobs.get(normalizedStreamId) || recoverStreamJob(normalizedStreamId);
    if (!state) {
      return {
        events: [],
        done: true,
        error: { code: "stream_not_found", message: "browser stream no longer exists" }
      };
    }
    advanceRecoveredStream(state);
    if (state.error) {
      streamJobs.delete(state.streamId);
      clearStreamRecovery(state.streamId);
      return { events: [], done: true, error: state.error };
    }

    const events = [];
    if (!state.toolCalls) {
      const current = String(state.answer || "");
      if (state.delivered && !current.startsWith(state.delivered)) {
        state.error = {
          code: "stream_rewrite_detected",
          message: "provider rewrote text inside the committed stream prefix after " + Array.from(state.delivered).length + " emitted characters"
        };
        streamJobs.delete(state.streamId);
        clearStreamRecovery(state.streamId);
        return { events: [], done: true, error: state.error };
      }

      const streamable = guardedStreamPrefix(current, state.done);
      if (streamable.startsWith(state.delivered)) {
        const delta = streamable.slice(state.delivered.length);
        if (delta) {
          const payload = state.roleEmitted
            ? { content: delta }
            : { role: "assistant", content: delta };
          state.roleEmitted = true;
          state.delivered = streamable;
          events.push(streamChunk(state, payload));
        }
      }
    }

    if (state.done && !state.finalEmitted) {
      if (state.toolCalls) {
        events.push(streamChunk(state, { role: "assistant", tool_calls: state.toolCalls }, "tool_calls"));
      } else {
        events.push(streamChunk(state, {}, "stop"));
      }
      state.finalEmitted = true;
    }

    const done = state.done && state.finalEmitted;
    const result = {
      events,
      done,
      error: null,
      progress_seq: state.progressSeq,
      progress_phase: state.progressPhase
    };
    if (done) {
      streamJobs.delete(state.streamId);
      clearStreamRecovery(state.streamId);
    } else {
      saveStreamRecovery(state);
    }
    return result;
  };

  const cancelStreamJob = (streamId) => {
    const normalizedStreamId = String(streamId || "");
    const state = streamJobs.get(normalizedStreamId) || recoverStreamJob(normalizedStreamId);
    if (!state) return { cancelled: false, missing: true };
    state.cancelled = true;
    try { queryVisible(state.context, "completion")?.click(); } catch (_) {}
    return { cancelled: true };
  };

  globalThis.__LLMGATEWAY_ADAPTER__ = {
    meta: {
      contract_version: CONTRACT_VERSION,
      id: "chatgpt-web",
      provider: "chatgpt",
      adapter_version: ADAPTER_VERSION
    },

    async probe(context) {
      const probeTimeoutMs = Number(context?.probe_timeout_ms || 8000);
      const hostOk = await waitForExpectedHost("chatgpt.com", probeTimeoutMs);
      if (!hostOk) {
        const currentHost = String(location.hostname || "").trim() || "<empty>";
        return { ok: false, code: "wrong_page", message: "Expected chatgpt.com but found " + currentHost };
      }
      await waitFor(
        () => queryVisible(context, "input") || loginIndicator(context),
        probeTimeoutMs
      );
      const composer = queryVisible(context, "input");
      const loginVisible = Boolean(loginIndicator(context));
      if (loginVisible) {
        return {
          ok: false,
          code: "login_required",
          message: "ChatGPT login is required before the adapter can run."
        };
      }
      if (!composer) {
        return {
          ok: false,
          code: "adapter_incompatible",
          message: "ChatGPT prompt composer was not found; the web UI may have changed."
        };
      }
      return {
        ok: true,
        code: "ready",
        message: "ChatGPT composer detected",
        page_signature: "chatgpt-composer-v1"
      };
    },

    async streamStart(request, context) {
      return startStreamJob(request, context);
    },

    async streamPoll(request) {
      return pollStreamJob(request?.stream_id);
    },

    async streamCancel(request) {
      return cancelStreamJob(request?.stream_id);
    },

    async chat(request, context) {
      await selectModel(context);
      await startNewConversation(context);
      const composer = await waitFor(() => queryVisible(context, "input"), 15000);
      if (!composer) throw new Error("ADAPTER_INCOMPATIBLE: ChatGPT prompt composer disappeared");

      const before = await captureResponseBaseline(context);
      const prompt = formatMessages(request);
      if (!prompt.trim()) throw new Error("INVALID_REQUEST: no textual messages to submit");
      await setComposer(composer, prompt);
      await attachImages(request, context);
      await submitComposer(context, composer);

      const startedAt = Date.now();
      let last = "";
      let stableSince = 0;
      let answer = "";
      while (Date.now() - startedAt < Number(context?.response_timeout_ms || 180000)) {
        if (loginIndicator(context)) {
          throw new Error("LOGIN_REQUIRED: ChatGPT session expired while waiting for a response");
        }
        const responses = responseLeaves(context);
        const candidate = newResponseText(before, responses);
        const responseAdvanced = Boolean(candidate);
        const generating = Boolean(queryVisible(context, "completion"));
        if (!responseAdvanced) {
          await sleep(180);
          continue;
        }
        if (candidate && candidate !== last) {
          last = candidate;
          answer = candidate;
          stableSince = Date.now();
        } else if (candidate && !generating && stableSince && Date.now() - stableSince >= Number(context?.response_stable_ms || 1200)) {
          answer = candidate;
          break;
        }
        await sleep(180);
      }
      if (!answer) throw new Error("RESPONSE_TIMEOUT: ChatGPT did not produce a readable response");

      const body = openAIResult(
        request,
        request?.model || context?.model_label || "chatgpt-web",
        answer,
        "chatcmpl_chatgpt_web_"
      );

      if (request?.stream) {
        return {
          status: 200,
          content_type: "text/event-stream",
          body: "data: " + JSON.stringify(body) + "\n\ndata: [DONE]\n\n"
        };
      }
      return { status: 200, content_type: "application/json", body };
    }
  };
})();
