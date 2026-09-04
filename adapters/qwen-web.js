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

  const formatMessages = (request) => {
    const messages = Array.isArray(request?.messages) ? request.messages : [];
    return messages.map((message) => {
      const role = String(message?.role || "user");
      let content = message?.content;
      if (Array.isArray(content)) {
        content = content.map((part) => {
          if (typeof part === "string") return part;
          if (part?.type === "text" || part?.type === "input_text") return part.text || "";
          return "";
        }).filter(Boolean).join("\n");
      }
      return role.toUpperCase() + ": " + String(content || "");
    }).join("\n\n");
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

  globalThis.__LLMGATEWAY_ADAPTER__ = {
    meta: {
      contract_version: CONTRACT_VERSION,
      id: "qwen-web",
      provider: "qwen",
      adapter_version: ADAPTER_VERSION
    },

    async probe(context) {
      const hostOk = location.hostname === "chat.qwen.ai";
      const composer = await waitFor(() => queryFirst(context, "input"), Number(context?.probe_timeout_ms || 8000));
      const loginVisible = Boolean(queryFirst(context, "login"));
      if (!hostOk) {
        return { ok: false, code: "wrong_page", message: "Expected chat.qwen.ai but found " + location.hostname };
      }
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

    async chat(request, context) {
      await selectModel(context);
      const composer = await waitFor(() => queryFirst(context, "input"), 15000);
      if (!composer) throw new Error("ADAPTER_INCOMPATIBLE: Qwen prompt composer disappeared");

      const before = responseTexts(context);
      const prompt = formatMessages(request);
      if (!prompt.trim()) throw new Error("INVALID_REQUEST: no textual messages to submit");
      setComposer(composer, prompt);

      const send = await waitFor(() => queryFirst(context, "send"), 5000);
      if (!send) throw new Error("ADAPTER_INCOMPATIBLE: Qwen send control was not found");
      send.click();

      const startedAt = Date.now();
      let last = "";
      let stableSince = 0;
      let answer = "";
      while (Date.now() - startedAt < Number(context?.response_timeout_ms || 180000)) {
        const responses = responseTexts(context);
        const candidate = responses.length > before.length ? responses[responses.length - 1] : responses[responses.length - 1] || "";
        const generating = Boolean(queryFirst(context, "completion"));
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

      const body = {
        id: "chatcmpl_qwen_web_" + Date.now(),
        object: request?.stream ? "chat.completion.chunk" : "chat.completion",
        model: request?.model || context?.model_label || "qwen-web",
        choices: request?.stream
          ? [{ index: 0, delta: { role: "assistant", content: answer }, finish_reason: "stop" }]
          : [{ index: 0, message: { role: "assistant", content: answer }, finish_reason: "stop" }]
      };
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
