// llmgateway built-in Gemini Web adapter.
// Contract v1. Runs inside an authenticated gemini.google.com page through loopback CDP.
// Authentication, CAPTCHA, 2FA, anti-abuse controls, and provider quotas remain interactive/provider-owned.
(() => {
  const CONTRACT_VERSION = 1;
  const ADAPTER_VERSION = "2026.09.04";

  const defaults = {
    input: [
      "div[aria-label='Enter a prompt for Gemini']",
      "div.ql-editor[role='textbox'][contenteditable='true']",
      "rich-textarea [contenteditable='true']",
      "[contenteditable='true'][role='textbox']"
    ],
    send: [
      "button[aria-label='Send message']",
      "button.send-button",
      ".send-button"
    ],
    response: [
      "div.markdown.markdown-main-panel",
      "model-response message-content",
      "model-response .model-response-text",
      "div.response-container",
      "div.presented-response-container"
    ],
    completion: [
      "button[aria-label='Stop response']",
      "button[aria-label='Stop generating']",
      "button[aria-label*='Stop']"
    ],
    login: [
      "a[href*='accounts.google.com']",
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

  const responseTexts = (context) => queryAll(context, "response").map(text).filter(Boolean);

  globalThis.__LLMGATEWAY_ADAPTER__ = {
    meta: {
      contract_version: CONTRACT_VERSION,
      id: "gemini-web",
      provider: "gemini",
      adapter_version: ADAPTER_VERSION
    },

    async probe(context) {
      const hostOk = location.hostname === "gemini.google.com";
      const composer = await waitFor(() => queryFirst(context, "input"), Number(context?.probe_timeout_ms || 8000));
      const loginVisible = Boolean(queryFirst(context, "login"));
      if (!hostOk) {
        return { ok: false, code: "wrong_page", message: "Expected gemini.google.com but found " + location.hostname };
      }
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

    async chat(request, context) {
      await selectModel(context);
      const composer = await waitFor(() => queryFirst(context, "input"), 15000);
      if (!composer) throw new Error("ADAPTER_INCOMPATIBLE: Gemini prompt composer disappeared");

      const before = responseTexts(context);
      const prompt = formatMessages(request);
      if (!prompt.trim()) throw new Error("INVALID_REQUEST: no textual messages to submit");
      setComposer(composer, prompt);

      const send = await waitFor(() => queryFirst(context, "send"), 5000);
      if (!send) throw new Error("ADAPTER_INCOMPATIBLE: Gemini send control was not found");
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
      if (!answer) throw new Error("RESPONSE_TIMEOUT: Gemini did not produce a readable response");

      const body = {
        id: "chatcmpl_gemini_web_" + Date.now(),
        object: request?.stream ? "chat.completion.chunk" : "chat.completion",
        model: request?.model || context?.model_label || "gemini-web",
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
