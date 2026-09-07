import fs from "node:fs";

const source = fs.readFileSync("ui/account-intelligence.js", "utf8");

function requireText(text, message) {
  if (!source.includes(text)) throw new Error(message);
}

requireText("account.account_runtime || {}", "Accounts UI must consume account runtime diagnostics");
requireText("runtime.queue_depth", "Accounts UI must expose bounded queue state");
requireText("runtime.concurrency_limit", "Accounts UI must expose effective concurrency");
requireText("runtime.generation", "Accounts UI must expose runtime generation");
requireText("readiness.browser_direct_ready", "Accounts UI must distinguish direct-ready browser accounts");
requireText("readiness.browser_running", "Accounts UI must distinguish warm/headless browser state");
requireText("browser_transport?.auth_generation", "Accounts UI must expose auth generation metadata");
requireText("browser_transport?.transport_plan?.activation_cost", "Accounts UI must expose browser activation cost");
requireText("transport_plan?.activation_reason", "Accounts UI must explain browser activation cost");
requireText('"Headless active"', "Accounts UI must label warm invisible browser execution");
requireText('"Browser cold"', "Accounts UI must label cold browser activation state");
requireText('"Cooling down"', "Accounts UI must label route cooldown state explicitly");
requireText('"Queued"', "Accounts UI must label active admission queueing");

console.log("provider runtime UI contract: ok");
