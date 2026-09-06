import fs from "node:fs";
import vm from "node:vm";
import assert from "node:assert/strict";

const source = fs.readFileSync(new URL("../ui/voice.js", import.meta.url), "utf8");
const sandbox = { globalThis: {} };
sandbox.globalThis = sandbox;
vm.runInNewContext(source, sandbox);

const { parseCommand } = sandbox.LLMGatewayVoice;
assert.deepEqual({ ...parseCommand("create new chat") }, { type: "new_thread" });
assert.deepEqual({ ...parseCommand("stop generation") }, { type: "stop_generation" });
assert.deepEqual({ ...parseCommand("try again") }, { type: "retry" });
assert.deepEqual({ ...parseCommand("send current draft") }, { type: "send_current_draft" });
assert.deepEqual(
  { ...parseCommand("switch to model Gemini 3.1 Pro") },
  { type: "set_model", query: "gemini 3.1 pro" }
);
assert.deepEqual(
  { ...parseCommand("attach file quarterly report.pdf") },
  { type: "attach_artifact", query: "quarterly report.pdf" }
);
for (const unsafe of [
  "delete account primary",
  "disable model gpt",
  "run shell rm",
  "change api key",
  "open arbitrary url",
]) {
  assert.equal(parseCommand(unsafe), null, unsafe);
}
console.log("voice command allowlist UI contract passed");
