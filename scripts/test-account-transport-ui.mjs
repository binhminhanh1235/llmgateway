import fs from "node:fs";

const source = fs.readFileSync("ui/app.js", "utf8");

function requireText(text, message) {
  if (!source.includes(text)) throw new Error(message);
}

requireText("data-toggle-browserless", "Accounts UI must render a Browserless toggle");
requireText('transport_policy: desired ? "browserless-preferred" : "browser-only"', "UI must send logical transport policies");
requireText('"direct-http": "Direct HTTP"', "UI must distinguish Direct HTTP effective transport");
requireText('"browser-fallback": "Browser fallback"', "UI must distinguish browser fallback");
requireText('"unavailable": "Unavailable"', "UI must expose unavailable effective transport");
requireText("capability.supported === true", "UI must disable unsupported browserless controls");
requireText("checkbox.checked = !desired", "UI must restore toggle state after update failure");
requireText("/_llmgateway/accounts/", "UI must use the provider-neutral account transport endpoint");

const start = source.indexOf("function accountTransportHtml");
const end = source.indexOf("async function refreshAccountModels", start);
if (start < 0 || end < 0) throw new Error("transport UI functions are missing");
const transportUi = source.slice(start, end);
for (const providerSpecific of ["browser-gemini", "browser-chatgpt", "browser-qwen"]) {
  if (transportUi.includes(providerSpecific)) {
    throw new Error(`Accounts transport UI must not branch on ${providerSpecific}`);
  }
}

console.log("account transport UI contract: ok");
