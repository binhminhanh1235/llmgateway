import fs from "node:fs";

const source = fs.readFileSync("ui/browser-control.js", "utf8");
const css = fs.readFileSync("ui/browser-control.css", "utf8");

function requireText(haystack, needle, message) {
  if (!haystack.includes(needle)) throw new Error(message);
}

requireText(
  source,
  'request("/_llmgateway/browser-runtime/settings")',
  "Accounts UI must load browser runtime discovery"
);
requireText(
  source,
  'method: "PATCH"',
  "Browser runtime selection must persist through PATCH"
);
requireText(
  source,
  'body: JSON.stringify({ browser_id: browserId })',
  "Browser runtime selection must send the detected browser id"
);
requireText(
  source,
  'Auto · Google Chrome preferred',
  "Auto browser mode must communicate Chrome preference"
);
for (const label of ["Google Chrome", "Edge", "Brave", "Chromium"]) {
  requireText(
    source,
    label,
    `Browser selector guidance must include ${label}`
  );
}
requireText(
  source,
  "data-browser-runtime-select",
  "Accounts UI must render a browser runtime select control"
);
requireText(
  source,
  "Applies on next browser launch",
  "UI must explain hot selection boundary"
);
requireText(
  css,
  ".browser-runtime-settings",
  "Browser selector must have dedicated layout"
);
requireText(
  css,
  ".browser-runtime-picker select",
  "Browser selector must have select styling"
);

console.log("browser runtime selector UI contract: ok");
