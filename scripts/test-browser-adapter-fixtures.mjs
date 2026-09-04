import fs from "node:fs";
import vm from "node:vm";
import assert from "node:assert/strict";

class FakeElement {
  constructor(text = "") {
    this.innerText = text;
    this.textContent = text;
    this.value = "";
    this.parentElement = null;
  }
  focus() {}
  click() {}
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
  globalThis.location = { hostname: host };
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

  installPage({ host: "gemini.google.com", nodes: {} });
  adapter = loadAdapter("adapters/gemini-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, false);
  assert.equal(probe.code, "adapter_incompatible");

  installPage({
    host: "gemini.google.com",
    nodes: { "a[href*='accounts.google.com']": new FakeElement("Sign in") }
  });
  adapter = loadAdapter("adapters/gemini-web.js");
  probe = await adapter.probe({ probe_timeout_ms: 20 });
  assert.equal(probe.ok, false);
  assert.equal(probe.code, "login_required");
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

await testGemini();
await testQwen();
console.log("built-in Gemini/Qwen fake-page adapter fixtures passed");
