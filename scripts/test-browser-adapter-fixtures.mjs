import fs from "node:fs";
import vm from "node:vm";
import assert from "node:assert/strict";

class FakeElement {
  constructor(text = "") {
    this.innerText = text;
    this.textContent = text;
    this.value = "";
    this.parentElement = null;
    this.hidden = false;
  }
  focus() {}
  click() { if (typeof this.onClick === "function") this.onClick(); }
  dispatchEvent() { return true; }
  getAttribute() { return null; }
  closest() { return this; }
}

class FakeTextAreaElement extends FakeElement {}
class FakeInputElement extends FakeElement {}
class FakeSessionStorage {
  constructor() { this.values = new Map(); }
  getItem(key) { return this.values.has(key) ? this.values.get(key) : null; }
  setItem(key, value) { this.values.set(String(key), String(value)); }
  removeItem(key) { this.values.delete(String(key)); }
  clear() { this.values.clear(); }
}

globalThis.HTMLTextAreaElement = FakeTextAreaElement;
globalThis.HTMLInputElement = FakeInputElement;
globalThis.InputEvent = class {
  constructor(type, init = {}) { this.type = type; Object.assign(this, init); }
};
globalThis.Event = class {
  constructor(type, init = {}) { this.type = type; Object.assign(this, init); }
};
globalThis.KeyboardEvent = class {
  constructor(type, init = {}) { this.type = type; Object.assign(this, init); }
};
globalThis.sessionStorage = new FakeSessionStorage();

function installPage({ host, path = "/app", nodes = {} }) {
  globalThis.location = { hostname: host, pathname: path };
  globalThis.document = {
    querySelector(selector) {
      return nodes[selector] ?? null;
    },
    querySelectorAll(selector) {
      const value = nodes[selector];
      if (Array.isArray(value)) return value;
      return value ? [value] : [];
    },
    dispatchEvent() { return true; }
  };
}

function loadAdapter(path) {
  delete globalThis.__LLMGATEWAY_ADAPTER__;
  const source = fs.readFileSync(path, "utf8");
  vm.runInThisContext(source, { filename: path });
  assert.ok(globalThis.__LLMGATEWAY_ADAPTER__, path + " did not register an adapter");
  return globalThis.__LLMGATEWAY_ADAPTER__;
}

async function testGemini() {
  const input = new FakeElement();
  installPage({
    host: "gemini.google.com",
    nodes: { "div[aria-label='Enter a prompt for Gemini']": input }
  });
  let adapter = loadAdapter("adapters/gemini-web.js");
  assert.equal(adapter.meta.contract_version, 1);
  assert.equal(adapter.meta.id, "gemini-web");
  let probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, true, probe.message);
  assert.equal(probe.page_signature, "gemini-composer-v1");

  installPage({
    host: "gemini.google.com",
    nodes: { "[aria-label='Enter a prompt here']": input }
  });
  adapter = loadAdapter("adapters/gemini-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, true, probe.message);

  installPage({
    host: "",
    nodes: { "div[aria-label='Enter a prompt for Gemini']": input }
  });
  adapter = loadAdapter("adapters/gemini-web.js");
  setTimeout(() => { globalThis.location.hostname = "gemini.google.com"; }, 25);
  probe = await adapter.probe({ probe_timeout_ms: 200 });
  assert.equal(probe.ok, true, probe.message);
  assert.equal(probe.page_signature, "gemini-composer-v1");

  installPage({ host: "example.test", nodes: {} });
  adapter = loadAdapter("adapters/gemini-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, false);
  assert.equal(probe.code, "wrong_page");
  assert.match(probe.message, /example\.test/);

  installPage({ host: "gemini.google.com", nodes: {} });
  adapter = loadAdapter("adapters/gemini-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, false);
  assert.equal(probe.code, "adapter_incompatible");

  const hiddenLogin = new FakeElement("Sign in");
  hiddenLogin.hidden = true;
  installPage({
    host: "gemini.google.com",
    nodes: { "a[href*='accounts.google.com/ServiceLogin']": hiddenLogin }
  });
  adapter = loadAdapter("adapters/gemini-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, false);
  assert.equal(probe.code, "adapter_incompatible");

  installPage({
    host: "gemini.google.com",
    nodes: { "a[href*='accounts.google.com/ServiceLogin']": new FakeElement("Sign in") }
  });
  adapter = loadAdapter("adapters/gemini-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, false);
  assert.equal(probe.code, "login_required");
}

async function testChatGPT() {
  const input = new FakeElement();
  installPage({
    host: "chatgpt.com",
    path: "/",
    nodes: { "#prompt-textarea": input }
  });
  let adapter = loadAdapter("adapters/chatgpt-web.js");
  assert.equal(adapter.meta.contract_version, 1);
  assert.equal(adapter.meta.id, "chatgpt-web");
  assert.equal(adapter.meta.provider, "chatgpt");
  let probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, true, probe.message);
  assert.equal(probe.page_signature, "chatgpt-composer-v1");

  installPage({
    host: "chatgpt.com",
    path: "/",
    nodes: {
      "#prompt-textarea": input,
      "a[href*='/auth/login']": new FakeElement("Log in")
    }
  });
  adapter = loadAdapter("adapters/chatgpt-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, false);
  assert.equal(probe.code, "login_required");

  installPage({ host: "example.test", path: "/", nodes: {} });
  adapter = loadAdapter("adapters/chatgpt-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, false);
  assert.equal(probe.code, "wrong_page");
}

async function testQwen() {
  const input = new FakeTextAreaElement();
  installPage({
    host: "chat.qwen.ai",
    nodes: { "textarea.message-input-textarea": input }
  });
  let adapter = loadAdapter("adapters/qwen-web.js");
  assert.equal(adapter.meta.contract_version, 1);
  assert.equal(adapter.meta.id, "qwen-web");
  let probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, true, probe.message);
  assert.equal(probe.page_signature, "qwen-composer-v1");

  installPage({ host: "chat.qwen.ai", nodes: {} });
  adapter = loadAdapter("adapters/qwen-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, false);
  assert.equal(probe.code, "adapter_incompatible");

  installPage({
    host: "chat.qwen.ai",
    nodes: { "a[href*='login']": new FakeElement("Log in") }
  });
  adapter = loadAdapter("adapters/qwen-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, false);
  assert.equal(probe.code, "login_required");
}


async function testGeminiToolBridge() {
  const input = new FakeElement();
  const response = new FakeElement("");
  const send = new FakeElement("Send");
  const nodes = {
    "div[aria-label='Enter a prompt for Gemini']": input,
    "button[aria-label='Send message']": send
  };
  send.onClick = () => {
    response.innerText = '[[LLMGATEWAY_TOOL_CALLS]]{"tool_calls":[{"name":"read_file","arguments":{"path":"src/main.rs"}}]}[[/LLMGATEWAY_TOOL_CALLS]]';
    response.textContent = response.innerText;
    nodes["div.markdown.markdown-main-panel"] = [response];
  };
  installPage({
    host: "gemini.google.com",
    nodes
  });
  const adapter = loadAdapter("adapters/gemini-web.js");
  const result = await adapter.chat({
    model: "gemini-web-test",
    stream: false,
    messages: [{ role: "user", content: "Read src/main.rs" }],
    tools: [{
      type: "function",
      function: {
        name: "read_file",
        description: "Read a repository file",
        parameters: {
          type: "object",
          properties: { path: { type: "string" } },
          required: ["path"]
        }
      }
    }]
  }, { response_timeout_ms: 4000 });

  assert.equal(result.status, 200);
  const choice = result.body.choices[0];
  assert.equal(choice.finish_reason, "tool_calls");
  assert.equal(choice.message.tool_calls[0].function.name, "read_file");
  assert.deepEqual(
    JSON.parse(choice.message.tool_calls[0].function.arguments),
    { path: "src/main.rs" }
  );
}

async function testChatGPTToolBridge() {
  const input = new FakeElement();
  const response = new FakeElement("");
  const send = new FakeElement("Send");
  const nodes = {
    "#prompt-textarea": input,
    "#composer-submit-button": send
  };
  send.onClick = () => {
    response.innerText = '[[LLMGATEWAY_TOOL_CALLS]]{"tool_calls":[{"name":"read_file","arguments":{"path":"README.md"}}]}[[/LLMGATEWAY_TOOL_CALLS]]';
    response.textContent = response.innerText;
    nodes["[data-message-author-role='assistant'] .markdown"] = [response];
  };
  installPage({ host: "chatgpt.com", path: "/", nodes });
  const adapter = loadAdapter("adapters/chatgpt-web.js");
  const result = await adapter.chat({
    model: "chatgpt-web-test",
    stream: false,
    messages: [{ role: "user", content: "Read README.md" }],
    tools: [{
      type: "function",
      function: {
        name: "read_file",
        description: "Read a repository file",
        parameters: {
          type: "object",
          properties: { path: { type: "string" } },
          required: ["path"]
        }
      }
    }]
  }, { response_timeout_ms: 2000, response_stable_ms: 60 });

  assert.equal(result.status, 200);
  const choice = result.body.choices[0];
  assert.equal(choice.finish_reason, "tool_calls");
  assert.equal(choice.message.tool_calls[0].function.name, "read_file");
  assert.deepEqual(
    JSON.parse(choice.message.tool_calls[0].function.arguments),
    { path: "README.md" }
  );
}

async function testChatGPTModelPickerFlow() {
  const input = new FakeElement();
  const response = new FakeElement("");
  const trigger = new FakeElement("Instant");
  const configure = new FakeElement("Configure...");
  const modal = new FakeElement("Intelligence");
  const thinking = new FakeElement("Thinking For complex questions");
  const send = new FakeElement("Send");
  const nodes = {
    "#prompt-textarea": input,
    "button.__composer-pill": trigger,
    "#composer-submit-button": send
  };
  let selected = false;
  trigger.onClick = () => {
    nodes["[data-testid='model-configure-modal']"] = configure;
  };
  configure.onClick = () => {
    nodes["[data-testid='modal-intelligence-menu']"] = modal;
    nodes["[data-testid='modal-intelligence-menu'] button[role='radio']"] = thinking;
  };
  thinking.onClick = () => { selected = true; };
  send.onClick = () => {
    response.innerText = "model-selected";
    response.textContent = response.innerText;
    nodes["[data-message-author-role='assistant'] .markdown"] = [response];
  };

  installPage({ host: "chatgpt.com", path: "/", nodes });
  const adapter = loadAdapter("adapters/chatgpt-web.js");
  const result = await adapter.chat({
    model: "chatgpt-web-test",
    stream: false,
    messages: [{ role: "user", content: "Use Thinking" }]
  }, {
    model_label: "Thinking",
    response_timeout_ms: 2000,
    response_stable_ms: 60
  });

  assert.equal(selected, true);
  assert.equal(result.body.choices[0].message.content, "model-selected");
}

async function testChatGPTFreshThreadForcesNewChat() {
  const input = new FakeElement();
  const oldResponse = new FakeElement("stale restored response");
  const newResponse = new FakeElement("fresh answer");
  const newChat = new FakeElement("New chat");
  const send = new FakeElement("Send");
  const nodes = {
    "#prompt-textarea": input,
    "#composer-submit-button": send,
    "a[data-testid='create-new-chat-button']": newChat,
    "[data-message-author-role='assistant'] .markdown": [oldResponse]
  };
  newChat.onClick = () => {
    globalThis.location.pathname = "/";
    nodes["[data-message-author-role='assistant'] .markdown"] = [];
  };
  send.onClick = () => {
    nodes["[data-message-author-role='assistant'] .markdown"] = [newResponse];
  };

  installPage({ host: "chatgpt.com", path: "/c/restored", nodes });
  const adapter = loadAdapter("adapters/chatgpt-web.js");
  const result = await adapter.chat({
    model: "chatgpt-web-test",
    stream: false,
    messages: [{ role: "user", content: "fresh logical thread" }]
  }, {
    start_new_conversation: true,
    response_timeout_ms: 1500,
    response_stable_ms: 60
  });

  assert.equal(result.body.choices[0].message.content, "fresh answer");
  assert.equal(globalThis.location.pathname, "/");
}

async function testChatGPTFreshStreamSkipsRedundantNewChatAndSubmitsBeforeReturn() {
  const input = new FakeElement();
  const response = new FakeElement("");
  const newChat = new FakeElement("New chat");
  const send = new FakeElement("Send");
  const nodes = {
    "#prompt-textarea": input,
    "#composer-submit-button": send,
    "a[data-testid='create-new-chat-button']": newChat,
    "[data-message-author-role='assistant'] .markdown": []
  };
  let newChatClicks = 0;
  let sendClicks = 0;
  newChat.onClick = () => {
    newChatClicks += 1;
    globalThis.location.pathname = "/";
  };
  send.onClick = () => {
    sendClicks += 1;
    response.innerText = "S".repeat(180);
    response.textContent = response.innerText;
    nodes["[data-message-author-role='assistant'] .markdown"] = [response];
  };

  installPage({ host: "chatgpt.com", path: "/", nodes });
  const adapter = loadAdapter("adapters/chatgpt-web.js");
  const started = await adapter.streamStart({
    model: "chatgpt-web-test",
    stream: true,
    messages: [{ role: "user", content: "fresh stream" }]
  }, {
    start_new_conversation: true,
    response_timeout_ms: 1500,
    response_stable_ms: 60
  });

  assert.equal(newChatClicks, 0, "fresh ChatGPT target must not navigate through New chat again");
  assert.equal(sendClicks, 1, "streamStart must submit before returning its stream id");
  assert.ok(started.stream_id);

  await new Promise((resolve) => setTimeout(resolve, 120));
  let polled = await adapter.streamPoll({ stream_id: started.stream_id });
  assert.equal(polled.error, null, JSON.stringify(polled));
  assert.ok(polled.events.length >= 1, JSON.stringify(polled));

  if (!polled.done) {
    await new Promise((resolve) => setTimeout(resolve, 120));
    polled = await adapter.streamPoll({ stream_id: started.stream_id });
    assert.equal(polled.error, null, JSON.stringify(polled));
  }
  assert.equal(polled.done, true, JSON.stringify(polled));
}

async function testChatGPTStreamRecoversAfterDocumentReplacement() {
  const input = new FakeElement();
  const response = new FakeElement("");
  const send = new FakeElement("Send");
  const finalText = "Recovered ChatGPT stream. ".repeat(12);
  const nodes = {
    "#prompt-textarea": input,
    "#composer-submit-button": send,
    "[data-message-author-role='assistant'] .markdown": []
  };

  send.onClick = () => {
    globalThis.location.pathname = "/c/recovered-stream";
    response.innerText = finalText;
    response.textContent = finalText;
    nodes["[data-message-author-role='assistant'] .markdown"] = [response];
  };

  installPage({ host: "chatgpt.com", path: "/", nodes });
  let adapter = loadAdapter("adapters/chatgpt-web.js");
  const started = await adapter.streamStart({
    model: "chatgpt-web-test",
    stream: true,
    messages: [{ role: "user", content: "recover after navigation" }]
  }, {
    start_new_conversation: true,
    response_timeout_ms: 1500,
    response_stable_ms: 60
  });

  assert.ok(started.stream_id);
  await new Promise((resolve) => setTimeout(resolve, 180));

  // Simulate a hard document replacement: the page-side Map disappears,
  // while same-tab sessionStorage and the rendered native conversation survive.
  delete globalThis.__LLMGATEWAY_STREAM_JOBS__;
  installPage({ host: "chatgpt.com", path: "/c/recovered-stream", nodes });
  adapter = loadAdapter("adapters/chatgpt-web.js");

  const emitted = [];
  let polled = await adapter.streamPoll({ stream_id: started.stream_id });
  assert.equal(polled.error, null, JSON.stringify(polled));
  emitted.push(...polled.events.map((event) => event.choices?.[0]?.delta?.content || "").filter(Boolean));

  if (!polled.done) {
    await new Promise((resolve) => setTimeout(resolve, 90));
    polled = await adapter.streamPoll({ stream_id: started.stream_id });
    assert.equal(polled.error, null, JSON.stringify(polled));
    emitted.push(...polled.events.map((event) => event.choices?.[0]?.delta?.content || "").filter(Boolean));
  }

  assert.equal(polled.done, true, JSON.stringify(polled));
  assert.equal(emitted.join(""), finalText);
  assert.equal(polled.events.at(-1).choices[0].finish_reason, "stop");
}

async function testChatGPTReopenWaitsForStableHistory() {
  const input = new FakeElement();
  const oldOne = new FakeElement("old one");
  const oldTwo = new FakeElement("old two");
  const answer = new FakeElement("new answer");
  const send = new FakeElement("Send");
  const nodes = {
    "#prompt-textarea": input,
    "#composer-submit-button": send,
    "[data-message-author-role='assistant'] .markdown": [oldOne]
  };

  setTimeout(() => {
    nodes["[data-message-author-role='assistant'] .markdown"] = [oldOne, oldTwo];
  }, 30);

  send.onClick = () => {
    setTimeout(() => {
      oldOne.innerText = "old one with late citation";
      oldOne.textContent = oldOne.innerText;
    }, 20);
    setTimeout(() => {
      nodes["[data-message-author-role='assistant'] .markdown"] = [oldOne, oldTwo, answer];
    }, 60);
  };

  installPage({ host: "chatgpt.com", path: "/c/native-thread", nodes });
  const adapter = loadAdapter("adapters/chatgpt-web.js");
  const result = await adapter.chat({
    model: "chatgpt-web-test",
    stream: false,
    messages: [{ role: "user", content: "continue native thread" }]
  }, {
    reuse_native_conversation: true,
    history_hydration_timeout_ms: 1500,
    history_stable_ms: 80,
    response_timeout_ms: 1500,
    response_stable_ms: 60
  });

  assert.equal(result.body.choices[0].message.content, "new answer");
}

async function testQwenToolBridgeStream() {
  const input = new FakeTextAreaElement();
  const response = new FakeElement("");
  const send = new FakeElement("Send");
  send.onClick = () => {
    response.innerText = '[[LLMGATEWAY_TOOL_CALLS]]{"tool_calls":[{"name":"run_tests","arguments":{"scope":"unit"}}]}[[/LLMGATEWAY_TOOL_CALLS]]';
    response.textContent = response.innerText;
  };
  installPage({
    host: "chat.qwen.ai",
    nodes: {
      "textarea.message-input-textarea": input,
      "button.send-button": send,
      "div.response-message-content.phase-answer": response
    }
  });
  const adapter = loadAdapter("adapters/qwen-web.js");
  const result = await adapter.chat({
    model: "qwen-web-test",
    stream: true,
    messages: [{ role: "user", content: "Run unit tests" }],
    tools: [{
      type: "function",
      function: {
        name: "run_tests",
        description: "Run tests",
        parameters: {
          type: "object",
          properties: { scope: { type: "string" } }
        }
      }
    }]
  }, { response_timeout_ms: 4000 });

  assert.equal(result.content_type, "text/event-stream");
  const dataLine = result.body.split("\n").find((line) => line.startsWith("data: {"));
  const chunk = JSON.parse(dataLine.slice(6));
  const choice = chunk.choices[0];
  assert.equal(choice.finish_reason, "tool_calls");
  assert.equal(choice.delta.tool_calls[0].function.name, "run_tests");
  assert.deepEqual(
    JSON.parse(choice.delta.tool_calls[0].function.arguments),
    { scope: "unit" }
  );
}



async function testQwenIncrementalStream() {
  const input = new FakeTextAreaElement();
  const response = new FakeElement("");
  const send = new FakeElement("Send");
  const firstText = "A".repeat(180);
  const finalText = firstText + "B".repeat(100);
  send.onClick = () => {
    response.innerText = firstText;
    response.textContent = response.innerText;
    setTimeout(() => {
      response.innerText = finalText;
      response.textContent = response.innerText;
    }, 180);
  };
  installPage({
    host: "chat.qwen.ai",
    nodes: {
      "textarea.message-input-textarea": input,
      "button.send-button": send,
      "div.response-message-content.phase-answer": response
    }
  });
  const adapter = loadAdapter("adapters/qwen-web.js");
  const started = await adapter.streamStart({
    model: "qwen-web-test",
    stream: true,
    messages: [{ role: "user", content: "Say hello" }]
  }, { response_timeout_ms: 4000 });

  const emitted = [];
  await new Promise((resolve) => setTimeout(resolve, 140));
  const first = await adapter.streamPoll({ stream_id: started.stream_id });
  assert.equal(first.error, null);
  assert.equal(first.done, false);
  assert.ok(first.events.length >= 1, JSON.stringify(first));
  emitted.push(...first.events.map((event) => event.choices?.[0]?.delta?.content || "").filter(Boolean));

  await new Promise((resolve) => setTimeout(resolve, 180));
  const second = await adapter.streamPoll({ stream_id: started.stream_id });
  assert.equal(second.error, null);
  assert.equal(second.done, false);
  emitted.push(...second.events.map((event) => event.choices?.[0]?.delta?.content || "").filter(Boolean));

  await new Promise((resolve) => setTimeout(resolve, 1100));
  const final = await adapter.streamPoll({ stream_id: started.stream_id });
  assert.equal(final.error, null);
  assert.equal(final.done, true);
  emitted.push(...final.events.map((event) => event.choices?.[0]?.delta?.content || "").filter(Boolean));
  assert.equal(emitted.join(""), finalText);
  assert.equal(final.events.at(-1).choices[0].finish_reason, "stop");
}

async function testBrowserTailRewriteTolerance() {
  const cases = [
    {
      path: "adapters/gemini-web.js",
      host: "gemini.google.com",
      pathName: "/app",
      inputSelector: "div[aria-label='Enter a prompt for Gemini']",
      sendSelector: "button[aria-label='Send message']",
      stopSelector: "button[aria-label='Stop response']",
      responseSelector: "div.markdown.markdown-main-panel",
      input: new FakeElement()
    },
    {
      path: "adapters/chatgpt-web.js",
      host: "chatgpt.com",
      pathName: "/",
      inputSelector: "#prompt-textarea",
      sendSelector: "#composer-submit-button",
      stopSelector: "button[data-testid='stop-button']",
      responseSelector: "[data-message-author-role='assistant'] .markdown",
      input: new FakeElement()
    },
    {
      path: "adapters/qwen-web.js",
      host: "chat.qwen.ai",
      pathName: "/app",
      inputSelector: "textarea.message-input-textarea",
      sendSelector: "button.send-button",
      stopSelector: "button.stop-button",
      responseSelector: "div.response-message-content.phase-answer",
      input: new FakeTextAreaElement()
    }
  ];

  const stablePrefix = "Stable browser-stream section. ".repeat(12);
  const draftAnswer = stablePrefix + "Draft tail that the provider is still composing.";
  const finalAnswer = stablePrefix + "Corrected tail after the provider rerendered the active sentence.";

  for (const fixture of cases) {
    const response = new FakeElement("");
    const send = new FakeElement("Send");
    const stop = new FakeElement("Stop");
    const nodes = {
      [fixture.inputSelector]: fixture.input,
      [fixture.sendSelector]: send,
      [fixture.stopSelector]: stop
    };
    send.onClick = () => {
      response.innerText = draftAnswer;
      response.textContent = response.innerText;
      nodes[fixture.responseSelector] = response;
      setTimeout(() => {
        response.innerText = finalAnswer;
        response.textContent = response.innerText;
      }, 220);
      setTimeout(() => {
        delete nodes[fixture.stopSelector];
      }, 420);
    };

    installPage({ host: fixture.host, path: fixture.pathName, nodes });
    const adapter = loadAdapter(fixture.path);
    const started = await adapter.streamStart({
      model: "browser-rewrite-test",
      stream: true,
      messages: [{ role: "user", content: "Generate a paragraph" }]
    }, { response_timeout_ms: 5000, response_stable_ms: 250 });

    const emitted = [];
    await new Promise((resolve) => setTimeout(resolve, 160));
    const first = await adapter.streamPoll({ stream_id: started.stream_id });
    assert.equal(first.error, null, JSON.stringify(first));
    emitted.push(...first.events.map((event) => event.choices?.[0]?.delta?.content || "").filter(Boolean));

    await new Promise((resolve) => setTimeout(resolve, 180));
    const rewritten = await adapter.streamPoll({ stream_id: started.stream_id });
    assert.equal(rewritten.error, null, JSON.stringify(rewritten));
    assert.equal(rewritten.done, false, JSON.stringify(rewritten));
    emitted.push(...rewritten.events.map((event) => event.choices?.[0]?.delta?.content || "").filter(Boolean));

    await new Promise((resolve) => setTimeout(resolve, 550));
    let final = await adapter.streamPoll({ stream_id: started.stream_id });
    if (!final.done) {
      await new Promise((resolve) => setTimeout(resolve, 350));
      final = await adapter.streamPoll({ stream_id: started.stream_id });
    }
    assert.equal(final.error, null, JSON.stringify(final));
    assert.equal(final.done, true, JSON.stringify(final));
    emitted.push(...final.events.map((event) => event.choices?.[0]?.delta?.content || "").filter(Boolean));
    assert.equal(emitted.join(""), finalAnswer, fixture.path);
  }
}

async function testCommittedPrefixRewriteStillFails() {
  const input = new FakeTextAreaElement();
  const response = new FakeElement("");
  const send = new FakeElement("Send");
  const stop = new FakeElement("Stop");
  const initial = "A".repeat(320);
  send.onClick = () => {
    response.innerText = initial;
    response.textContent = response.innerText;
    setTimeout(() => {
      response.innerText = "B" + initial.slice(1);
      response.textContent = response.innerText;
    }, 220);
  };
  installPage({
    host: "chat.qwen.ai",
    nodes: {
      "textarea.message-input-textarea": input,
      "button.send-button": send,
      "button.stop-button": stop,
      "div.response-message-content.phase-answer": response
    }
  });
  const adapter = loadAdapter("adapters/qwen-web.js");
  const started = await adapter.streamStart({
    model: "qwen-rewrite-test",
    stream: true,
    messages: [{ role: "user", content: "Generate a long answer" }]
  }, { response_timeout_ms: 4000 });

  await new Promise((resolve) => setTimeout(resolve, 160));
  const first = await adapter.streamPoll({ stream_id: started.stream_id });
  assert.equal(first.error, null, JSON.stringify(first));
  assert.ok(first.events.some((event) => event.choices?.[0]?.delta?.content), JSON.stringify(first));

  await new Promise((resolve) => setTimeout(resolve, 180));
  const rewritten = await adapter.streamPoll({ stream_id: started.stream_id });
  assert.equal(rewritten.done, true, JSON.stringify(rewritten));
  assert.equal(rewritten.error?.code, "stream_rewrite_detected", JSON.stringify(rewritten));
  assert.match(rewritten.error?.message || "", /committed stream prefix/);
}


async function testBrowserStreamProgressHeartbeats() {
  const cases = [
    {
      path: "adapters/gemini-web.js",
      host: "gemini.google.com",
      inputSelector: "div[aria-label='Enter a prompt for Gemini']",
      sendSelector: "button[aria-label='Send message']",
      stopSelector: "button[aria-label='Stop response']",
      input: new FakeElement()
    },
    {
      path: "adapters/chatgpt-web.js",
      host: "chatgpt.com",
      inputSelector: "#prompt-textarea",
      sendSelector: "#composer-submit-button",
      stopSelector: "button[data-testid='stop-button']",
      input: new FakeElement()
    },
    {
      path: "adapters/qwen-web.js",
      host: "chat.qwen.ai",
      inputSelector: "textarea.message-input-textarea",
      sendSelector: "button.send-button",
      stopSelector: "button.stop-button",
      input: new FakeTextAreaElement()
    }
  ];

  for (const fixture of cases) {
    const send = new FakeElement("Send");
    const stop = new FakeElement("Stop");
    const nodes = {
      [fixture.inputSelector]: fixture.input,
      [fixture.sendSelector]: send,
      [fixture.stopSelector]: stop
    };
    installPage({ host: fixture.host, path: fixture.host === "chatgpt.com" ? "/" : "/app", nodes });
    const adapter = loadAdapter(fixture.path);
    const started = await adapter.streamStart({
      model: "browser-heartbeat-test",
      stream: true,
      messages: [{ role: "user", content: "think before answering" }]
    }, { response_timeout_ms: 4000 });

    await new Promise((resolve) => setTimeout(resolve, 180));
    const first = await adapter.streamPoll({ stream_id: started.stream_id });
    assert.equal(first.error, null, JSON.stringify(first));
    assert.equal(first.events.length, 0, JSON.stringify(first));
    assert.equal(typeof first.progress_seq, "number", JSON.stringify(first));
    assert.ok(first.progress_seq >= 2, JSON.stringify(first));

    await new Promise((resolve) => setTimeout(resolve, 1100));
    const second = await adapter.streamPoll({ stream_id: started.stream_id });
    assert.equal(second.error, null, JSON.stringify(second));
    assert.equal(second.events.length, 0, JSON.stringify(second));
    assert.ok(second.progress_seq > first.progress_seq, JSON.stringify({ first, second }));
    assert.match(String(second.progress_phase || ""), /generating|submitted-wait/);

    const cancelled = await adapter.streamCancel({ stream_id: started.stream_id });
    assert.equal(cancelled.cancelled, true);
  }
}

async function testGeminiStreamCancellation() {
  const input = new FakeElement();
  const response = new FakeElement("");
  const stop = new FakeElement("Stop");
  const send = new FakeElement("Send");
  let stopped = false;
  stop.onClick = () => { stopped = true; };
  send.onClick = () => {
    response.innerText = "Partial";
    response.textContent = response.innerText;
  };
  installPage({
    host: "gemini.google.com",
    nodes: {
      "div[aria-label='Enter a prompt for Gemini']": input,
      "button[aria-label='Send message']": send,
      "button[aria-label='Stop response']": stop,
      "div.markdown.markdown-main-panel": response
    }
  });
  const adapter = loadAdapter("adapters/gemini-web.js");
  const started = await adapter.streamStart({
    model: "gemini-web-test",
    stream: true,
    messages: [{ role: "user", content: "Keep talking" }]
  }, { response_timeout_ms: 4000 });

  await new Promise((resolve) => setTimeout(resolve, 140));
  const cancelled = await adapter.streamCancel({ stream_id: started.stream_id });
  assert.equal(cancelled.cancelled, true);
  assert.equal(stopped, true);

  await new Promise((resolve) => setTimeout(resolve, 160));
  const polled = await adapter.streamPoll({ stream_id: started.stream_id });
  assert.equal(polled.done, true);
  assert.equal(polled.error.code, "cancelled");
}



async function testGeminiFreshThreadForcesNewChat() {
  const input = new FakeElement();
  const oldResponse = new FakeElement("stale restored response");
  const newResponse = new FakeElement("fresh answer");
  const newChat = new FakeElement("New chat");
  const send = new FakeElement("Send");
  const nodes = {
    "div[aria-label='Enter a prompt for Gemini']": input,
    "button[aria-label='Send message']": send,
    "button[aria-label='New chat']": newChat,
    "div.markdown.markdown-main-panel": [oldResponse]
  };
  newChat.onClick = () => {
    globalThis.location.pathname = "/app";
    nodes["div.markdown.markdown-main-panel"] = [];
  };
  send.onClick = () => {
    nodes["div.markdown.markdown-main-panel"] = [newResponse];
  };

  installPage({ host: "gemini.google.com", path: "/app/restored", nodes });
  const adapter = loadAdapter("adapters/gemini-web.js");
  const result = await adapter.chat({
    model: "gemini-web-test",
    stream: false,
    messages: [{ role: "user", content: "fresh logical thread" }]
  }, {
    start_new_conversation: true,
    response_timeout_ms: 1500,
    response_stable_ms: 60
  });

  assert.equal(result.body.choices[0].message.content, "fresh answer");
  assert.equal(globalThis.location.pathname, "/app");
}

async function testGeminiReopenWaitsForStableHistoryAndIgnoresRerenderedOldTurns() {
  const input = new FakeElement();
  const oldOne = new FakeElement("old one");
  const oldTwo = new FakeElement("old two");
  const answer = new FakeElement("new answer");
  const send = new FakeElement("Send");
  const nodes = {
    "div[aria-label='Enter a prompt for Gemini']": input,
    "button[aria-label='Send message']": send,
    "div.markdown.markdown-main-panel": [oldOne]
  };

  setTimeout(() => {
    nodes["div.markdown.markdown-main-panel"] = [oldOne, oldTwo];
  }, 30);

  send.onClick = () => {
    setTimeout(() => {
      oldOne.innerText = "old one with late citation";
      oldOne.textContent = oldOne.innerText;
    }, 20);
    setTimeout(() => {
      nodes["div.markdown.markdown-main-panel"] = [oldOne, oldTwo, answer];
    }, 60);
  };

  installPage({ host: "gemini.google.com", path: "/app/native-thread", nodes });
  const adapter = loadAdapter("adapters/gemini-web.js");
  const result = await adapter.chat({
    model: "gemini-web-test",
    stream: false,
    messages: [{ role: "user", content: "continue native thread" }]
  }, {
    reuse_native_conversation: true,
    history_hydration_timeout_ms: 1500,
    history_stable_ms: 80,
    response_timeout_ms: 1500,
    response_stable_ms: 60
  });

  assert.equal(result.body.choices[0].message.content, "new answer");
}

async function testMidRequestLoginExpiry() {
  const input = new FakeElement();
  const nodes = {
    "div[aria-label='Enter a prompt for Gemini']": input,
    "button[aria-label='Send message']": new FakeElement("Send"),
    "div.markdown.markdown-main-panel": new FakeElement("")
  };
  nodes["button[aria-label='Send message']"].onClick = () => {
    nodes["a[href*='accounts.google.com/ServiceLogin']"] = new FakeElement("Sign in");
  };
  installPage({ host: "gemini.google.com", nodes });
  const adapter = loadAdapter("adapters/gemini-web.js");
  await assert.rejects(
    () => adapter.chat({
      model: "gemini-web-test",
      stream: false,
      messages: [{ role: "user", content: "Continue" }]
    }, { response_timeout_ms: 2000 }),
    /LOGIN_REQUIRED: Gemini session expired/
  );
}

await testGemini();
await testChatGPT();
await testQwen();
await testGeminiToolBridge();
await testChatGPTToolBridge();
await testChatGPTModelPickerFlow();
await testChatGPTFreshThreadForcesNewChat();
await testChatGPTFreshStreamSkipsRedundantNewChatAndSubmitsBeforeReturn();
await testChatGPTStreamRecoversAfterDocumentReplacement();
await testChatGPTReopenWaitsForStableHistory();
await testQwenToolBridgeStream();
await testQwenIncrementalStream();
await testBrowserTailRewriteTolerance();
await testCommittedPrefixRewriteStillFails();
await testBrowserStreamProgressHeartbeats();
await testGeminiStreamCancellation();
await testGeminiFreshThreadForcesNewChat();
await testGeminiReopenWaitsForStableHistoryAndIgnoresRerenderedOldTurns();
await testMidRequestLoginExpiry();
console.log("built-in Gemini/ChatGPT/Qwen fake-page adapter fixtures passed");
