import fs from "node:fs";

const app = fs.readFileSync("ui/app.js", "utf8");
const groups = fs.readFileSync("ui/model-groups.js", "utf8");
const html = fs.readFileSync("ui/index.html", "utf8");

function requireText(source, text, message) {
  if (!source.includes(text)) throw new Error(message);
}

requireText(app, 'accountStatus: "enabled"', "Accounts must default to the Enabled tab");
requireText(app, 'modelStatus: "enabled"', "Models must default to the Enabled tab");
requireText(
  app,
  '(model) => model.fallback_eligible === true',
  "Model tab counts must use effective fallback eligibility"
);
requireText(
  app,
  'const effectiveEnabled = model.fallback_eligible === true;',
  "Model filtering and card state must use effective fallback eligibility"
);
requireText(
  app,
  '(globalEnabled ? "Disabled by account" : "Disabled")',
  "Models disabled only by account/binding state must be labeled distinctly"
);
requireText(
  groups,
  'status: "enabled"',
  "Groups must default to the Enabled tab"
);
requireText(
  groups,
  'state.models.filter((model) => model.fallback_eligible === true)',
  "Group member picker must only list effectively enabled fallback models"
);
requireText(
  groups,
  'const active = new Set(state.models.map((model) => model.id));',
  "Existing inactive group members must remain preservable in fallback order"
);

for (const kind of ["account", "model", "group"]) {
  requireText(
    html,
    `class="status-tab active" type="button" data-${kind}-status="enabled"`,
    `${kind} status tabs must render Enabled as the default active tab`
  );
}

console.log("status synchronization UI contract: ok");
