import fs from "node:fs";
import assert from "node:assert/strict";

const html = fs.readFileSync(new URL("../ui/index.html", import.meta.url), "utf8");
const app = fs.readFileSync(new URL("../ui/app.js", import.meta.url), "utf8");

for (const id of ["micButton", "voiceModeButton", "voiceStatus", "generateImageButton"]) {
  assert.match(html, new RegExp(`id=["']${id}["']`), id);
}
assert.match(html, /\/ui\/voice\.js/);
assert.match(app, /new MediaRecorder/);
assert.match(app, /\/v1\/audio\/transcriptions/);
assert.match(app, /\/v1\/images\/generations/);
assert.match(app, /\/v1\/images\/edits/);
assert.match(app, /LLMGatewayVoice\?\.parseCommand/);
assert.match(app, /new AbortController/);
assert.match(app, /capabilitySummary/);
assert.match(app, /generated-image-grid/);

console.log("multimodal media UI contract passed");
