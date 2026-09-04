// llmgateway built-in Qwen Web adapter.
// Contract v1. Runs inside an authenticated chat.qwen.ai page through loopback CDP.
(() => {
  const CONTRACT_VERSION = 1;
  const ADAPTER_VERSION = "2026.09.04";

  const defaults = {
    input: [
      "textarea.message-input-textarea",
      "textarea[placeholder='How can I help you today?']",
      "textarea"
    ],
    send: [
      "button.send-button",
      ".chat-prompt-send-button button",
      "div.message-input-right-button-send button",
      "button[type='submit']"
    ],
    response: [
      "div.response-message-content.phase-answer",
      "div.response-message-content",
      "div.chat-response-message",
      "div[id^='chat-response-message-']"
    ],
    responseMonitor: [
      "div.response-message-content.phase-answer"
    ],
    completion: [
      "button.stop-button"
    ],
    login: [
      "a[href*='login']",
      "a[href*='signin']"
    ],
    modelTrigger: [
      "[class*='model-selector-text']",
      ".model-selector-text"
    ],
    modelOptions: [
      ".ant-select-item-option",
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
  const waitFor = async (fn, timeoutMs = 30000, pollMs = 120) => {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      const value = fn();
      if (value) return value;
      await sleep(pollMs);
    }
    return null;
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
    const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
    if (setter && node instanceof HTMLTextAreaElement) setter.call(node, value);
    else node.value = value;
    node.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: value }));
    node.dispatchEvent(new Event("change", { bubbles: true }));
  };

  const selectModel = async (context) => {
    const desired = String(context?.model_label || "").trim();
    if (!desired) return null;
    const desiredNorm = normalize(desired);
    const label = queryFirst(context, "modelTrigger");
    if (!label) throw new Error("MODEL_PICKER_NOT_FOUND: Qwen model picker is not visible");

    const current = normalize(text(label));
    if (current.includes(desiredNorm) || desiredNorm.includes(current)) return desired;

    const clickable = label.closest("button,[role='button'],.ant-select") || label.parentElement || label;
    clickable.click();
    await sleep(250);
    const candidates = queryAll(context, "modelOptions");
    const match = candidates.find((node) => {
      const candidate = normalize(text(node));
      return candidate && (candidate.includes(desiredNorm) || desiredNorm.includes(candidate));
    });
    if (!match) throw new Error("MODEL_NOT_FOUND: Qwen model not available in picker: " + desired);
    match.click();
    await sleep(300);
    return desired;
  };

  const responseTexts = (context) => {
    const monitored = queryAll(context, "responseMonitor").map(text).filter(Boolean);
    if (monitored.length) return monitored;
    return queryAll(context, "response").map(text).filter(Boolean);
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

  const streamChunk = (state, delta, finishReason = null) => ({
    id: state.completionId,
    object: "chat.completion.chunk",
    model: state.model,
    choices: [{ index: 0, delta, finish_reason: finishReason }]
  });

  const startStreamJob = (request, context) => {
    const streamId = "qwen_stream_" + Date.now() + "_" + Math.random().toString(36).slice(2);
    const state = {
      streamId,
      completionId: "chatcmpl_qwen_web_" + Date.now(),
      model: request?.model || context?.model_label || "qwen-web",
      request,
      context,
      answer: "",
      delivered: "",
      done: false,
      finalEmitted: false,
      roleEmitted: false,
      cancelled: false,
      error: null,
      toolCalls: null
    };
    streamJobs.set(streamId, state);

    Promise.resolve().then(async () => {
      try {
        await selectModel(context);
        const composer = await waitFor(() => queryFirst(context, "input"), 15000);
        if (!composer) throw new Error("ADAPTER_INCOMPATIBLE: Qwen prompt composer disappeared");

        const before = responseTexts(context);
        const baselineText = before[before.length - 1] || "";
        const prompt = formatMessages(request);
        if (!prompt.trim()) throw new Error("INVALID_REQUEST: no textual messages to submit");
        setComposer(composer, prompt);

        const send = await waitFor(() => queryFirst(context, "send"), 5000);
        if (!send) {
          if (queryFirst(context, "login")) throw new Error("LOGIN_REQUIRED: Qwen session expired");
          throw new Error("ADAPTER_INCOMPATIBLE: Qwen send control was not found");
        }
        send.click();

        const startedAt = Date.now();
        let last = "";
        let stableSince = 0;
        let answer = "";
        while (Date.now() - startedAt < Number(context?.response_timeout_ms || 180000)) {
          if (state.cancelled) {
            const stop = queryFirst(context, "completion");
            try { stop?.click(); } catch (_) {}
            throw new Error("STREAM_CANCELLED: client disconnected or cancelled");
          }
          if (queryFirst(context, "login")) {
            throw new Error("LOGIN_REQUIRED: Qwen session expired while waiting for a response");
          }
          const responses = responseTexts(context);
          const candidate = responses[responses.length - 1] || "";
          const responseAdvanced = responses.length > before.length || (candidate && candidate !== baselineText);
          const generating = Boolean(queryFirst(context, "completion"));
          if (!responseAdvanced) {
            await sleep(120);
            continue;
          }
          if (candidate && candidate !== last) {
            last = candidate;
            answer = candidate;
            state.answer = candidate;
            stableSince = Date.now();
          } else if (candidate && !generating && stableSince && Date.now() - stableSince >= 900) {
            answer = candidate;
            state.answer = candidate;
            break;
          }
          await sleep(120);
        }
        if (!answer) throw new Error("RESPONSE_TIMEOUT: Qwen did not produce a readable response");

        state.answer = answer;
        state.toolCalls = parseToolCalls(answer, request);
        state.done = true;
      } catch (error) {
        state.error = classifyStreamError(error);
        state.done = true;
      } finally {
        setTimeout(() => {
          if (streamJobs.get(streamId) === state) streamJobs.delete(streamId);
        }, 60000);
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
      if (current.startsWith(state.delivered)) {
        const delta = current.slice(state.delivered.length);
        if (delta) {
          const payload = state.roleEmitted
            ? { content: delta }
            : { role: "assistant", content: delta };
          state.roleEmitted = true;
          state.delivered = current;
          events.push(streamChunk(state, payload));
        }
      } else if (state.delivered) {
        state.error = {
          code: "stream_rewrite_detected",
          message: "provider rewrote text that was already emitted"
        };
        streamJobs.delete(state.streamId);
        return { events: [], done: true, error: state.error };
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
    if (done) streamJobs.delete(state.streamId);
    return { events, done, error: null };
  };

  const cancelStreamJob = (streamId) => {
    const state = streamJobs.get(String(streamId || ""));
    if (!state) return { cancelled: false, missing: true };
    state.cancelled = true;
    try { queryFirst(state.context, "completion")?.click(); } catch (_) {}
    return { cancelled: true };
  };

  globalThis.__LLMGATEWAY_ADAPTER__ = {
    meta: {
      contract_version: CONTRACT_VERSION,
      id: "qwen-web",
      provider: "qwen",
      adapter_version: ADAPTER_VERSION
    },

    async probe(context) {
      const hostOk = location.hostname === "chat.qwen.ai";
      if (!hostOk) {
        return { ok: false, code: "wrong_page", message: "Expected chat.qwen.ai but found " + location.hostname };
      }
      await waitFor(
        () => queryFirst(context, "input") || queryFirst(context, "login"),
        Number(context?.probe_timeout_ms || 8000)
      );
      const composer = queryFirst(context, "input");
      const loginVisible = Boolean(queryFirst(context, "login"));
      if (!composer) {
        return {
          ok: false,
          code: loginVisible ? "login_required" : "adapter_incompatible",
          message: loginVisible
            ? "Qwen login is required before the adapter can run."
            : "Qwen prompt composer was not found; the web UI may have changed."
        };
      }
      return {
        ok: true,
        code: "ready",
        message: "Qwen composer detected",
        page_signature: "qwen-composer-v1"
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
      const composer = await waitFor(() => queryFirst(context, "input"), 15000);
      if (!composer) throw new Error("ADAPTER_INCOMPATIBLE: Qwen prompt composer disappeared");

      const before = responseTexts(context);
      const baselineText = before[before.length - 1] || "";
      const prompt = formatMessages(request);
      if (!prompt.trim()) throw new Error("INVALID_REQUEST: no textual messages to submit");
      setComposer(composer, prompt);

      const send = await waitFor(() => queryFirst(context, "send"), 5000);
      if (!send) {
        if (queryFirst(context, "login")) throw new Error("LOGIN_REQUIRED: Qwen session expired");
        throw new Error("ADAPTER_INCOMPATIBLE: Qwen send control was not found");
      }
      send.click();

      const startedAt = Date.now();
      let last = "";
      let stableSince = 0;
      let answer = "";
      while (Date.now() - startedAt < Number(context?.response_timeout_ms || 180000)) {
        if (queryFirst(context, "login")) {
          throw new Error("LOGIN_REQUIRED: Qwen session expired while waiting for a response");
        }
        const responses = responseTexts(context);
        const candidate = responses[responses.length - 1] || "";
        const responseAdvanced = responses.length > before.length || (candidate && candidate !== baselineText);
        const generating = Boolean(queryFirst(context, "completion"));
        if (!responseAdvanced) {
          await sleep(180);
          continue;
        }
        if (candidate && candidate !== last) {
          last = candidate;
          answer = candidate;
          stableSince = Date.now();
        } else if (candidate && !generating && stableSince && Date.now() - stableSince >= 1200) {
          answer = candidate;
          break;
        }
        await sleep(180);
      }
      if (!answer) throw new Error("RESPONSE_TIMEOUT: Qwen did not produce a readable response");

      const body = openAIResult(
        request,
        request?.model || context?.model_label || "qwen-web",
        answer,
        "chatcmpl_qwen_web_"
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
