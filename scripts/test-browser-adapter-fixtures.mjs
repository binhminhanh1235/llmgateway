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

function installPage({ host, nodes = {} }) {
  globalThis.location = { hostname: host, pathname: "/" };
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
    nodes: {
      "#prompt-textarea": input,
      "a[href*='/auth/login']": new FakeElement("Log in")
    }
  });
  adapter = loadAdapter("adapters/chatgpt-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, false);
  assert.equal(probe.code, "login_required");

  installPage({ host: "example.test", nodes: {} });
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
  send.onClick = () => {
    response.innerText = '[[LLMGATEWAY_TOOL_CALLS]]{"tool_calls":[{"name":"read_file","arguments":{"path":"src/main.rs"}}]}[[/LLMGATEWAY_TOOL_CALLS]]';
    response.textContent = response.innerText;
  };
  installPage({
    host: "gemini.google.com",
    nodes: {
      "div[aria-label='Enter a prompt for Gemini']": input,
      "button[aria-label='Send message']": send,
      "div.markdown.markdown-main-panel": response
    }
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
  send.onClick = () => {
    response.innerText = '[[LLMGATEWAY_TOOL_CALLS]]{"tool_calls":[{"name":"read_file","arguments":{"path":"README.md"}}]}[[/LLMGATEWAY_TOOL_CALLS]]';
    response.textContent = response.innerText;
  };
  installPage({
    host: "chatgpt.com",
    nodes: {
      "#prompt-textarea": input,
      "button[data-testid='send-button']": send,
      "[data-message-author-role='assistant'] .markdown": response
    }
  });
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
  }, { response_timeout_ms: 4000 });

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
    "#composer-submit-button": send,
    "[data-message-author-role='assistant'] .markdown": response
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
  };

  installPage({ host: "chatgpt.com", nodes });
  const adapter = loadAdapter("adapters/chatgpt-web.js");
  const result = await adapter.chat({
    model: "chatgpt-web-test",
    stream: false,
    messages: [{ role: "user", content: "Use Thinking" }]
  }, {
    model_label: "Thinking",
    response_timeout_ms: 4000
  });

  assert.equal(selected, true);
  assert.equal(result.body.choices[0].message.content, "model-selected");
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
  send.onClick = () => {
    response.innerText = "Hello";
    response.textContent = response.innerText;
    setTimeout(() => {
      response.innerText = "Hello world";
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

  await new Promise((resolve) => setTimeout(resolve, 140));
  const first = await adapter.streamPoll({ stream_id: started.stream_id });
  assert.equal(first.error, null);
  assert.equal(first.done, false);
  assert.equal(first.events.length, 1, JSON.stringify(first));
  assert.equal(first.events[0].choices[0].delta.content, "Hello");

  await new Promise((resolve) => setTimeout(resolve, 180));
  const second = await adapter.streamPoll({ stream_id: started.stream_id });
  assert.equal(second.error, null);
  assert.equal(second.done, false);
  assert.equal(second.events.length, 1, JSON.stringify(second));
  assert.equal(second.events[0].choices[0].delta.content, " world");

  await new Promise((resolve) => setTimeout(resolve, 1000));
  const final = await adapter.streamPoll({ stream_id: started.stream_id });
  assert.equal(final.error, null);
  assert.equal(final.done, true);
  assert.equal(final.events.at(-1).choices[0].finish_reason, "stop");
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
await testQwenToolBridgeStream();
await testQwenIncrementalStream();
await testGeminiStreamCancellation();
await testMidRequestLoginExpiry();
console.log("built-in Gemini/ChatGPT/Qwen fake-page adapter fixtures passed");
