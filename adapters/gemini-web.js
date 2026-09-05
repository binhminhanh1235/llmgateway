// llmgateway built-in Gemini Web adapter.
// Contract v1. Runs inside an authenticated gemini.google.com page through loopback CDP.
// Authentication, CAPTCHA, 2FA, anti-abuse controls, and provider quotas remain interactive/provider-owned.
(() => {
  const CONTRACT_VERSION = 1;
  const ADAPTER_VERSION = "2026.09.05.1";

  const defaults = {
    input: [
      "div[aria-label='Enter a prompt for Gemini']",
      "[aria-label='Enter a prompt here']",
      "div.ql-editor[contenteditable='true']",
      "div.ql-editor",
      "div.ql-editor[role='textbox'][contenteditable='true']",
      "rich-textarea .ql-editor[contenteditable='true']",
      "rich-textarea [contenteditable='true']",
      "[contenteditable='true'][aria-label*='prompt' i]",
      "[contenteditable='true'][role='textbox']"
    ],
    fileInput: [
      "input[type='file'][accept*='image']",
      "input[type='file'][accept*='png']",
      "input[type='file']"
    ],
    send: [
      "button[aria-label='Send message']",
      "button[aria-label*='Send' i]",
      "button.send-button",
      ".send-button"
    ],
    newChat: [
      "button[aria-label='New chat']",
      "button[aria-label*='New chat' i]",
      "a[aria-label='New chat']",
      "a[aria-label*='New chat' i]",
      "[data-test-id='new-chat-button']",
      "a[href='/app']"
    ],
    response: [
      "div.markdown.markdown-main-panel",
      "model-response message-content"
    ],
    completion: [
      "button[aria-label='Stop response']",
      "button[aria-label='Stop generating']",
      "button[aria-label*='Stop']"
    ],
    login: [
      "a[href*='accounts.google.com/ServiceLogin']",
      "a[href*='accounts.google.com/v3/signin']",
      "a[href*='accounts.google.com/signin']",
      "form[action*='ServiceLogin']",
      "input[type='password']"
    ],
    modelTrigger: [
      "[data-test-id='model-selector']",
      "button[aria-label*='model' i]"
    ],
    modelOptions: [
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
  const loginIndicator = (context) => queryAll(context, "login").find(isVisible) || null;
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
    if (!input) throw new Error("ADAPTER_INCOMPATIBLE: Gemini image file input was not found");
    if (urls.length > 1 && input.multiple === false) {
      throw new Error("INVALID_REQUEST: Gemini composer currently accepts one image per file input");
    }
    const transfer = new DataTransfer();
    urls.forEach((url, index) => transfer.items.add(dataUrlFile(url, index)));
    try {
      input.files = transfer.files;
    } catch (_) {
      throw new Error("ADAPTER_INCOMPATIBLE: Gemini image file input rejected DataTransfer files");
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

  const setComposer = (node, value) => {
    node.focus();
    if (node instanceof HTMLTextAreaElement || node instanceof HTMLInputElement) {
      const proto = node instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, "value")?.set;
      if (setter) setter.call(node, value);
      else node.value = value;
    } else {
      node.textContent = value;
    }
    node.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: value }));
    node.dispatchEvent(new Event("change", { bubbles: true }));
  };

  const selectModel = async (context) => {
    const desired = String(context?.model_label || "").trim();
    if (!desired) return null;
    const desiredNorm = normalize(desired);

    let trigger = queryFirst(context, "modelTrigger");
    if (!trigger) {
      const buttons = [...document.querySelectorAll("button")];
      trigger = buttons.find((button) => /gemini|flash|pro|model/i.test(text(button))) || null;
    }
    if (!trigger) {
      throw new Error("MODEL_PICKER_NOT_FOUND: Gemini model picker is not visible");
    }
    const current = normalize(text(trigger) + " " + (trigger.getAttribute?.("aria-label") || ""));
    if (current.includes(desiredNorm) || desiredNorm.includes(current)) return desired;

    trigger.click();
    await sleep(250);
    let candidates = queryAll(context, "modelOptions");
    if (!candidates.length) {
      candidates = [...document.querySelectorAll("[role='menuitem'],[role='option']")];
    }
    const match = candidates.find((node) => {
      const candidate = normalize(text(node));
      return candidate && (candidate.includes(desiredNorm) || desiredNorm.includes(candidate));
    });
    if (!match) {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
      throw new Error("MODEL_NOT_FOUND: Gemini model not available in picker: " + desired);
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

  const captureResponseBaseline = async (context) => {
    if (!context?.reuse_native_conversation) {
      const leaves = responseLeaves(context);
      return { count: leaves.length, nodes: new Set(leaves) };
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
          return { count: leaves.length, nodes: new Set(leaves) };
        }
      } else {
        lastKey = key;
        lastLeaves = leaves;
        stableSince = leaves.length ? Date.now() : 0;
      }
      await sleep(120);
    }

    if (!lastLeaves.length) {
      throw new Error("ADAPTER_INCOMPATIBLE: Gemini conversation history did not load");
    }
    throw new Error("ADAPTER_INCOMPATIBLE: Gemini conversation history did not stabilize");
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
    const segments = String(location.pathname || "").split("/").filter(Boolean);
    const appIndex = segments.lastIndexOf("app");
    return appIndex >= 0 && appIndex === segments.length - 1;
  };

  const startNewConversation = async (context) => {
    if (!context?.start_new_conversation) return;
    const newChat = await waitFor(() => queryVisible(context, "newChat"), 8000);
    if (!newChat) {
      throw new Error("ADAPTER_INCOMPATIBLE: Gemini New chat control was not found");
    }
    newChat.click();
    const reset = await waitFor(
      () => freshChatLocation() && responseLeaves(context).length === 0,
      8000
    );
    if (!reset) {
      throw new Error("ADAPTER_INCOMPATIBLE: Gemini did not open a clean fresh chat");
    }
    await sleep(250);
  };

  const streamJobs = globalThis.__LLMGATEWAY_STREAM_JOBS__ || new Map();
  globalThis.__LLMGATEWAY_STREAM_JOBS__ = streamJobs;

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
  };

  const startStreamJob = (request, context) => {
    const streamId = "gemini_stream_" + Date.now() + "_" + Math.random().toString(36).slice(2);
    const state = {
      streamId,
      completionId: "chatcmpl_gemini_web_" + Date.now(),
      model: request?.model || context?.model_label || "gemini-web",
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
      lastProgressAt: Date.now()
    };
    streamJobs.set(streamId, state);

    Promise.resolve().then(async () => {
      try {
        await selectModel(context);
        markStreamProgress(state, "model-ready");
        await startNewConversation(context);
        markStreamProgress(state, "conversation-ready");
        const composer = await waitFor(() => queryVisible(context, "input"), 15000);
        if (!composer) throw new Error("ADAPTER_INCOMPATIBLE: Gemini prompt composer disappeared");
        markStreamProgress(state, "composer-ready");

        const before = await captureResponseBaseline(context);
        markStreamProgress(state, "history-ready");
        const prompt = formatMessages(request);
        if (!prompt.trim()) throw new Error("INVALID_REQUEST: no textual messages to submit");
        setComposer(composer, prompt);
        markStreamProgress(state, "prompt-ready");
        const attachedImages = await attachImages(request, context);
        if (attachedImages) markStreamProgress(state, "attachments-ready");

        const send = await waitFor(() => queryVisible(context, "send"), 5000);
        if (!send) {
          if (loginIndicator(context)) throw new Error("LOGIN_REQUIRED: Gemini session expired");
          throw new Error("ADAPTER_INCOMPATIBLE: Gemini send control was not found");
        }
        send.click();
        markStreamProgress(state, "submitted");

        const startedAt = Date.now();
        let last = "";
        let stableSince = 0;
        let answer = "";
        while (Date.now() - startedAt < Number(context?.response_timeout_ms || 180000)) {
          if (state.cancelled) {
            const stop = queryVisible(context, "completion");
            try { stop?.click(); } catch (_) {}
            throw new Error("STREAM_CANCELLED: client disconnected or cancelled");
          }
          if (loginIndicator(context)) {
            throw new Error("LOGIN_REQUIRED: Gemini session expired while waiting for a response");
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
            stableSince = Date.now();
            markStreamProgress(state, "response-advanced", true);
          } else if (candidate && !generating && stableSince && Date.now() - stableSince >= Number(context?.response_stable_ms || 900)) {
            answer = candidate;
            state.answer = candidate;
            break;
          }
          await sleep(120);
        }
        if (!answer) throw new Error("RESPONSE_TIMEOUT: Gemini did not produce a readable response");

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

  const pollStreamJob = (streamId) => {
    const state = streamJobs.get(String(streamId || ""));
    if (!state) {
      return {
        events: [],
        done: true,
        error: { code: "stream_not_found", message: "browser stream no longer exists" }
      };
    }
    if (state.error) {
      streamJobs.delete(state.streamId);
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
    if (done) streamJobs.delete(state.streamId);
    return result;
  };

  const cancelStreamJob = (streamId) => {
    const state = streamJobs.get(String(streamId || ""));
    if (!state) return { cancelled: false, missing: true };
    state.cancelled = true;
    try { queryVisible(state.context, "completion")?.click(); } catch (_) {}
    return { cancelled: true };
  };

  globalThis.__LLMGATEWAY_ADAPTER__ = {
    meta: {
      contract_version: CONTRACT_VERSION,
      id: "gemini-web",
      provider: "gemini",
      adapter_version: ADAPTER_VERSION
    },

    async probe(context) {
      const probeTimeoutMs = Number(context?.probe_timeout_ms || 8000);
      const hostOk = await waitForExpectedHost("gemini.google.com", probeTimeoutMs);
      if (!hostOk) {
        const currentHost = String(location.hostname || "").trim() || "<empty>";
        return { ok: false, code: "wrong_page", message: "Expected gemini.google.com but found " + currentHost };
      }
      await waitFor(
        () => queryVisible(context, "input") || loginIndicator(context),
        probeTimeoutMs
      );
      const composer = queryVisible(context, "input");
      const loginVisible = Boolean(loginIndicator(context));
      if (!composer) {
        return {
          ok: false,
          code: loginVisible ? "login_required" : "adapter_incompatible",
          message: loginVisible
            ? "Gemini login is required before the adapter can run."
            : "Gemini prompt composer was not found; the web UI may have changed."
        };
      }
      return {
        ok: true,
        code: "ready",
        message: "Gemini composer detected",
        page_signature: "gemini-composer-v1"
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
      if (!composer) throw new Error("ADAPTER_INCOMPATIBLE: Gemini prompt composer disappeared");

      const before = await captureResponseBaseline(context);
      const prompt = formatMessages(request);
      if (!prompt.trim()) throw new Error("INVALID_REQUEST: no textual messages to submit");
      setComposer(composer, prompt);
      await attachImages(request, context);

      const send = await waitFor(() => queryVisible(context, "send"), 5000);
      if (!send) {
        if (loginIndicator(context)) throw new Error("LOGIN_REQUIRED: Gemini session expired");
        throw new Error("ADAPTER_INCOMPATIBLE: Gemini send control was not found");
      }
      send.click();

      const startedAt = Date.now();
      let last = "";
      let stableSince = 0;
      let answer = "";
      while (Date.now() - startedAt < Number(context?.response_timeout_ms || 180000)) {
        if (loginIndicator(context)) {
          throw new Error("LOGIN_REQUIRED: Gemini session expired while waiting for a response");
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
      if (!answer) throw new Error("RESPONSE_TIMEOUT: Gemini did not produce a readable response");

      const body = openAIResult(
        request,
        request?.model || context?.model_label || "gemini-web",
        answer,
        "chatcmpl_gemini_web_"
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
