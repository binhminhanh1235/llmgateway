// llmgateway built-in Gemini Web adapter.
// Contract v1. Runs inside an authenticated gemini.google.com page through loopback CDP.
// Authentication, CAPTCHA, 2FA, anti-abuse controls, and provider quotas remain interactive/provider-owned.
(() => {
  const CONTRACT_VERSION = 1;
  const ADAPTER_VERSION = "2026.09.07.browser-fetch.1";

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
    const toolProtocolPending =
      Array.isArray(state.request?.tools) &&
      state.request.tools.length > 0 &&
      state.request?.tool_choice !== "none";
    if (!state.toolCalls && !toolProtocolPending) {
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
        if (toolProtocolPending) {
          const finalText = String(state.answer || "");
          const delta = finalText.slice(state.delivered.length);
          if (delta) {
            const payload = state.roleEmitted
              ? { content: delta }
              : { role: "assistant", content: delta };
            state.roleEmitted = true;
            state.delivered = finalText;
            events.push(streamChunk(state, payload));
          }
        }
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


  // P5 browser-context fetch bridge. The request executes inside gemini.google.com,
  // reusing browser cookies/origin. Explicit model recipes and native conversation
  // continuations remain on the UI adapter until their private wire metadata can be
  // preserved exactly; this bridge never silently substitutes another model.
  const geminiFetchJobs = globalThis.__LLMGATEWAY_GEMINI_FETCH_JOBS__ || new Map();
  globalThis.__LLMGATEWAY_GEMINI_FETCH_JOBS__ = geminiFetchJobs;
  const GEMINI_FETCH_DEFAULT_MODEL = "gemini-web-default";
  const GEMINI_FETCH_HOLD_CHARS = 192;

  const geminiFetchTextOnly = (request) => {
    if (Array.isArray(request?.tools) && request.tools.length) {
      throw new Error("BROWSER_FETCH_UNSUPPORTED: Gemini browser fetch defers tool-call requests to UI transport");
    }
    const messages = Array.isArray(request?.messages) ? request.messages : [];
    for (const message of messages) {
      const content = message?.content;
      if (!Array.isArray(content)) continue;
      for (const part of content) {
        if (typeof part === "string") continue;
        const type = String(part?.type || "");
        if (type && type !== "text" && type !== "input_text") {
          throw new Error("BROWSER_FETCH_UNSUPPORTED: Gemini browser fetch only handles text message parts");
        }
      }
    }
  };

  const geminiEmbeddedString = (html, key) => {
    const marker = '"' + key + '"';
    const start = String(html || "").indexOf(marker);
    if (start < 0) return "";
    const tail = String(html).slice(start + marker.length);
    const colon = tail.indexOf(":");
    if (colon < 0) return "";
    const value = tail.slice(colon + 1).trimStart();
    if (!value.startsWith('"')) return "";
    let escaped = false;
    for (let i = 1; i < value.length; i += 1) {
      const ch = value[i];
      if (escaped) { escaped = false; continue; }
      if (ch === "\\") { escaped = true; continue; }
      if (ch === '"') {
        try { return JSON.parse(value.slice(0, i + 1)); } catch (_) { return ""; }
      }
    }
    return "";
  };

  const geminiFetchUuid = () => {
    try { return String(globalThis.crypto?.randomUUID?.() || ("gemini-" + Date.now() + "-" + Math.random().toString(36).slice(2))).toUpperCase(); }
    catch (_) { return ("GEMINI-" + Date.now() + "-" + Math.random().toString(36).slice(2)).toUpperCase(); }
  };
  const geminiFetchReqId = () => 10000 + Math.floor(Math.random() * 90000);

  const geminiBuildInner = (prompt, language, requestUuid, temporary) => {
    const inner = Array(81).fill(null);
    inner[0] = [prompt, 0, null, null, null, null, 0];
    inner[1] = [language];
    inner[2] = ["", "", "", null, null, null, null, null, null, ""];
    inner[6] = [1];
    inner[7] = 1;
    inner[10] = 1;
    inner[11] = 0;
    inner[17] = [[0]];
    inner[18] = 0;
    inner[27] = 1;
    inner[30] = [4];
    inner[41] = [1];
    if (temporary) inner[45] = 1;
    inner[53] = 0;
    inner[59] = requestUuid;
    inner[61] = [];
    inner[68] = 1;
    inner[79] = 1;
    inner[80] = 1;
    return inner;
  };

  const geminiNested = (value, path) => {
    let current = value;
    for (const index of path) {
      if (!Array.isArray(current) || index >= current.length) return undefined;
      current = current[index];
    }
    return current;
  };

  const geminiParseFetchUpdate = (part) => {
    const errorCode = Number(geminiNested(part, [5, 2, 0, 1, 0]));
    const raw = geminiNested(part, [2]);
    if (typeof raw !== "string") {
      return { text: "", completed: false, errorCode: Number.isFinite(errorCode) ? errorCode : null };
    }
    let inner;
    try { inner = JSON.parse(raw); }
    catch (_) { return { text: "", completed: false, errorCode: Number.isFinite(errorCode) ? errorCode : null }; }
    const candidates = geminiNested(inner, [4]);
    let text = "";
    let completed = false;
    if (Array.isArray(candidates)) {
      for (const candidate of candidates) {
        if (!text) text = String(geminiNested(candidate, [1, 0]) || "");
        completed = completed || Number(geminiNested(candidate, [8, 0])) === 2;
      }
    }
    return { text, completed, errorCode: Number.isFinite(errorCode) ? errorCode : null };
  };

  const geminiParseAvailableFrames = (state, finalInput = false) => {
    let text = state.frameBuffer;
    let pos = 0;
    const frames = [];
    if (!state.preambleHandled) {
      const trimmed = text.replace(/^\s+/, "");
      const whitespace = text.length - trimmed.length;
      if (trimmed.length < 4 && ")]}'".startsWith(trimmed) && !finalInput) return frames;
      if (trimmed.startsWith(")]}'")) pos = whitespace + 4;
      state.preambleHandled = true;
    }
    while (true) {
      while (pos < text.length && /\s/.test(text[pos])) pos += 1;
      if (pos >= text.length) {
        state.frameBuffer = "";
        return frames;
      }
      const digitStart = pos;
      while (pos < text.length && /[0-9]/.test(text[pos])) pos += 1;
      if (digitStart === pos) throw new Error("BROWSER_FETCH_REJECTED: Gemini stream frame length marker was invalid");
      if (pos >= text.length) {
        state.frameBuffer = text.slice(digitStart);
        return frames;
      }
      if (text[pos] !== "\n") throw new Error("BROWSER_FETCH_REJECTED: Gemini stream frame length was not newline terminated");
      const length = Number(text.slice(digitStart, pos));
      if (!Number.isFinite(length) || length < 0) throw new Error("BROWSER_FETCH_REJECTED: Gemini stream frame length was invalid");
      const contentStart = pos;
      const contentEnd = contentStart + length;
      if (contentEnd > text.length) {
        state.frameBuffer = text.slice(digitStart);
        return frames;
      }
      const chunk = text.slice(contentStart, contentEnd).trim();
      pos = contentEnd;
      if (!chunk) continue;
      let value;
      try { value = JSON.parse(chunk); }
      catch (_) { throw new Error("BROWSER_FETCH_REJECTED: Gemini stream returned invalid JSON frame"); }
      if (Array.isArray(value)) frames.push(...value);
      else frames.push(value);
      if (pos >= text.length) {
        state.frameBuffer = "";
        return frames;
      }
    }
  };

  const geminiFetchChunk = (state, delta, finishReason = null) => ({
    id: state.completionId,
    object: "chat.completion.chunk",
    model: state.model,
    choices: [{ index: 0, delta, finish_reason: finishReason }]
  });

  const geminiFetchWakeReady = (state) => {
    if (state.readyResolved) return;
    if (!state.error && !state.done && state.events.length === 0) return;
    state.readyResolved = true;
    state.resolveReady?.();
  };

  const geminiObserveFetchText = (state, snapshot, completed) => {
    const value = String(snapshot || "");
    if (!value) return;
    if (!value.startsWith(state.emitted)) {
      throw new Error("BROWSER_FETCH_REJECTED: Gemini rewrote text outside the committed stability window");
    }
    state.latest = value;
    const chars = Array.from(value);
    const commitChars = completed ? chars.length : Math.max(0, chars.length - GEMINI_FETCH_HOLD_CHARS);
    const committed = chars.slice(0, commitChars).join("");
    if (committed.length <= state.emitted.length) return;
    const delta = committed.slice(state.emitted.length);
    state.emitted = committed;
    if (!delta) return;
    const payload = state.roleEmitted ? { content: delta } : { role: "assistant", content: delta };
    state.roleEmitted = true;
    state.events.push(geminiFetchChunk(state, payload));
    state.progressSeq += 1;
    state.progressPhase = "streaming";
    geminiFetchWakeReady(state);
  };

  const geminiApplyFetchFrame = (state, part) => {
    const update = geminiParseFetchUpdate(part);
    if (update.errorCode && update.errorCode !== 0) {
      if (update.errorCode === 1037) {
        throw new Error("RATE_LIMITED: Gemini web usage limit exceeded (error code 1037)");
      }
      if (update.errorCode === 1052) {
        throw new Error("MODEL_RECIPE_STALE: Gemini StreamGenerate rejected the selected model recipe (error code 1052)");
      }
      if (update.errorCode === 1155) {
        throw new Error("UPSTREAM_OVERLOADED: Gemini StreamGenerate returned transient rejection (error code 1155)");
      }
      throw new Error("BROWSER_FETCH_REJECTED: Gemini StreamGenerate returned error code " + update.errorCode);
    }
    if (update.text) state.latest = update.text;
    state.completed = state.completed || update.completed;
    geminiObserveFetchText(state, state.latest, state.completed);
  };

  const geminiPumpFetch = async (state, response) => {
    const reader = response.body?.getReader?.();
    if (!reader) throw new Error("BROWSER_FETCH_UNSUPPORTED: Gemini response body is not incrementally readable");
    const decoder = new TextDecoder();
    while (true) {
      if (state.cancelled) throw new Error("STREAM_CANCELLED: Gemini browser fetch cancelled");
      const { value, done } = await reader.read();
      if (done) break;
      state.frameBuffer += decoder.decode(value, { stream: true });
      for (const frame of geminiParseAvailableFrames(state, false)) geminiApplyFetchFrame(state, frame);
    }
    state.frameBuffer += decoder.decode();
    for (const frame of geminiParseAvailableFrames(state, true)) geminiApplyFetchFrame(state, frame);
    if (!state.completed) throw new Error("BROWSER_FETCH_REJECTED: Gemini StreamGenerate ended before completion marker");
    if (!state.latest.trim()) throw new Error("BROWSER_FETCH_REJECTED: Gemini StreamGenerate completed without assistant text");
    // Completion flushes the stability window even when the final provider frame
    // repeated the same text snapshot.
    geminiObserveFetchText(state, state.latest, true);
    state.done = true;
    state.progressSeq += 1;
    state.progressPhase = "completed";
    geminiFetchWakeReady(state);
  };

  const geminiFetchPrompt = (request) => {
    geminiFetchTextOnly(request);
    const prompt = formatMessages(request);
    if (!String(prompt || "").trim()) throw new Error("INVALID_REQUEST: no textual messages to submit");
    return prompt;
  };

  const startGeminiBrowserFetch = async (request, context) => {
    if (location.hostname !== "gemini.google.com") {
      throw new Error("BROWSER_FETCH_UNSUPPORTED: Gemini browser fetch requires gemini.google.com origin");
    }
    if (context?.thread_id_present) {
      throw new Error("BROWSER_FETCH_UNSUPPORTED: Gemini native conversation continuation stays on UI transport");
    }
    const model = String(request?.model || GEMINI_FETCH_DEFAULT_MODEL);
    if (model !== GEMINI_FETCH_DEFAULT_MODEL) {
      throw new Error("BROWSER_FETCH_UNSUPPORTED: Gemini selected-model private recipe stays on UI transport");
    }
    const prompt = geminiFetchPrompt(request);

    const bootstrap = await fetch("/app", { method: "GET", credentials: "include" });
    const html = await bootstrap.text();
    if (bootstrap.status === 401 || bootstrap.status === 403) {
      throw new Error("LOGIN_REQUIRED: Gemini browser session bootstrap was rejected");
    }
    if (bootstrap.status === 429) {
      throw new Error("RATE_LIMITED: Gemini browser bootstrap returned HTTP 429");
    }
    if (bootstrap.status === 503) {
      throw new Error("UPSTREAM_OVERLOADED: Gemini browser bootstrap returned HTTP 503");
    }
    if (!bootstrap.ok) {
      throw new Error("BROWSER_FETCH_REJECTED: Gemini browser bootstrap returned HTTP " + bootstrap.status);
    }
    const accessToken = geminiEmbeddedString(html, "SNlM0e");
    if (!accessToken) throw new Error("LOGIN_REQUIRED: Gemini browser bootstrap returned no access token");
    const buildLabel = geminiEmbeddedString(html, "cfb2h");
    const frontendSessionId = geminiEmbeddedString(html, "FdrFJe");
    const language = geminiEmbeddedString(html, "TuX5cc") || "en";
    const requestUuid = geminiFetchUuid();
    const inner = geminiBuildInner(prompt, language, requestUuid, Boolean(context?.ephemeral_chat));
    const fReq = JSON.stringify([null, JSON.stringify(inner)]);
    const query = new URLSearchParams({
      rt: "c",
      _reqid: String(geminiFetchReqId()),
      hl: language
    });
    if (buildLabel) query.set("bl", buildLabel);
    if (frontendSessionId) query.set("f.sid", frontendSessionId);
    const form = new URLSearchParams({ "f.req": fReq, at: accessToken });
    const controller = new AbortController();
    const response = await fetch("/_/BardChatUi/data/assistant.lamda.BardFrontendService/StreamGenerate?" + query.toString(), {
      method: "POST",
      credentials: "include",
      signal: controller.signal,
      headers: {
        "content-type": "application/x-www-form-urlencoded;charset=utf-8",
        "x-same-domain": "1",
        "x-goog-ext-525005358-jspb": JSON.stringify([requestUuid, 1]),
        "accept": "*/*"
      },
      body: form.toString()
    });
    if (response.status === 401 || response.status === 403) {
      throw new Error("LOGIN_REQUIRED: Gemini StreamGenerate rejected browser session");
    }
    if (response.status === 429) {
      throw new Error("RATE_LIMITED: Gemini StreamGenerate returned HTTP 429");
    }
    if (response.status === 503) {
      throw new Error("UPSTREAM_OVERLOADED: Gemini StreamGenerate returned HTTP 503");
    }
    if (!response.ok) {
      throw new Error("BROWSER_FETCH_REJECTED: Gemini StreamGenerate returned HTTP " + response.status);
    }

    const streamId = "gemini_fetch_" + Date.now() + "_" + Math.random().toString(36).slice(2);
    const state = {
      streamId,
      completionId: "chatcmpl_gemini_fetch_" + Date.now(),
      model,
      controller,
      events: [],
      emitted: "",
      latest: "",
      completed: false,
      done: false,
      finalEmitted: false,
      roleEmitted: false,
      cancelled: false,
      error: null,
      frameBuffer: "",
      preambleHandled: false,
      progressSeq: 1,
      progressPhase: "submitted",
      readyResolved: false,
      resolveReady: null,
      ready: null
    };
    state.ready = new Promise((resolve) => { state.resolveReady = resolve; });
    geminiFetchJobs.set(streamId, state);
    state.worker = geminiPumpFetch(state, response).catch((error) => {
      const message = String(error?.message || error);
      let code = "browser_fetch_rejected";
      if (/^STREAM_CANCELLED:/i.test(message)) code = "cancelled";
      else if (/^LOGIN_REQUIRED:/i.test(message)) code = "login_required";
      else if (/^RATE_LIMITED:/i.test(message)) code = "rate_limited";
      else if (/^UPSTREAM_OVERLOADED:/i.test(message)) code = "upstream_overloaded";
      else if (/^MODEL_RECIPE_STALE:/i.test(message)) code = "model_recipe_stale";
      state.error = { code, message };
      state.done = true;
      state.progressSeq += 1;
      state.progressPhase = state.error.code === "cancelled" ? "cancelled" : "failed";
      geminiFetchWakeReady(state);
    });
    const firstByteMs = Number(context?.first_byte_timeout_ms || 30000);
    await Promise.race([
      state.ready,
      sleep(firstByteMs).then(() => { throw new Error("BROWSER_FETCH_REJECTED: Gemini browser fetch first-byte timeout"); })
    ]);
    if (state.error) {
      geminiFetchJobs.delete(streamId);
      throw new Error(state.error.message);
    }
    return state;
  };

  const pollGeminiBrowserFetch = (streamId) => {
    const state = geminiFetchJobs.get(String(streamId || ""));
    if (!state) return { events: [], done: true, error: { code: "stream_not_found", message: "Gemini browser fetch stream not found" } };
    const events = state.events.splice(0);
    if (state.done && !state.error && !state.finalEmitted) {
      events.push(geminiFetchChunk(state, {}, "stop"));
      state.finalEmitted = true;
    }
    const done = state.done && (Boolean(state.error) || state.finalEmitted);
    const result = {
      events,
      done,
      error: state.error,
      progress_seq: state.progressSeq,
      progress_phase: state.progressPhase
    };
    if (done) geminiFetchJobs.delete(state.streamId);
    return result;
  };

  const cancelGeminiBrowserFetch = (streamId) => {
    const state = geminiFetchJobs.get(String(streamId || ""));
    if (!state) return { cancelled: false, missing: true };
    state.cancelled = true;
    try { state.controller?.abort?.(); } catch (_) {}
    state.error = { code: "cancelled", message: "Gemini browser fetch cancelled" };
    state.done = true;
    state.progressSeq += 1;
    state.progressPhase = "cancelled";
    geminiFetchWakeReady(state);
    return { cancelled: true };
  };

  const bufferedGeminiBrowserFetch = async (request, context) => {
    const state = await startGeminiBrowserFetch(request, context);
    await state.worker;
    if (state.error) {
      geminiFetchJobs.delete(state.streamId);
      throw new Error(state.error.message);
    }
    geminiFetchJobs.delete(state.streamId);
    return {
      status: 200,
      content_type: "application/json",
      body: openAIResult(request, state.model, state.latest, "chatcmpl_gemini_fetch_")
    };
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

    async browserFetch(request, context) {
      return bufferedGeminiBrowserFetch(request, context);
    },

    async streamStart(request, context) {
      if (context?.transport === "browser_fetch") {
        const state = await startGeminiBrowserFetch(request, context);
        return { stream_id: state.streamId, status: 200, content_type: "text/event-stream" };
      }
      return startStreamJob(request, context);
    },

    async streamPoll(request) {
      const streamId = String(request?.stream_id || "");
      if (streamId.startsWith("gemini_fetch_")) return pollGeminiBrowserFetch(streamId);
      return pollStreamJob(streamId);
    },

    async streamCancel(request) {
      const streamId = String(request?.stream_id || "");
      if (streamId.startsWith("gemini_fetch_")) return cancelGeminiBrowserFetch(streamId);
      return cancelStreamJob(streamId);
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
