(() => {
  const state = { groups: [], routes: [], editingId: null, tiers: [], loaded: false };
  const $ = (id) => document.getElementById(id);
  const e = {
    content: $("groupsContent"), create: $("createModelGroupButton"), refresh: $("refreshModelGroupsButton"),
    modal: $("modelGroupModal"), title: $("modelGroupModalTitle"), id: $("modelGroupIdInput"),
    tiers: $("modelGroupTiers"), addTier: $("addModelGroupTierButton"),
    save: $("saveModelGroupButton"), error: $("modelGroupError"),
  };
  const ui = () => window.llmgatewayUI;

  async function load(force = false) {
    if (!ui()?.apiFetch) return;
    if (state.loaded && !force) return render();
    e.content.innerHTML = '<div class="loading-box">Loading model groups…</div>';
    try {
      const response = await ui().apiFetch("/_llmgateway/model-groups");
      if (!response.ok) throw new Error(ui().extractError(await response.text(), response.status));
      const body = await response.json();
      state.groups = body.data || [];
      state.routes = body.routes || [];
      state.loaded = true;
      render();
    } catch (error) {
      e.content.innerHTML = '<div class="error-box">' + ui().escapeHtml(error.message || error) + "</div>";
    }
  }

  function routeLabel(id) {
    const r = state.routes.find((x) => x.id === id);
    return r ? r.id + " · " + r.provider + "/" + r.model + " · " + r.account : id;
  }

  function render() {
    if (!state.groups.length) {
      e.content.innerHTML = '<div class="loading-box">No model groups yet. Create one to define ordered fallback.</div>';
      return;
    }
    let html = "";
    for (const group of state.groups) {
      const badges = (group.is_default ? '<span class="badge available">Default</span>' : "") +
        '<span class="badge">' + (group.mode === "tiered" ? "Ordered tiers" : "Legacy flat") + "</span>";
      let body = "";
      if (group.mode === "tiered") {
        (group.tiers || []).forEach((tier, index) => {
          body += '<div class="group-tier-summary"><div class="group-tier-number">' + (index + 1) +
            '</div><div><strong>Priority ' + Number(tier.priority) + "</strong><span>" +
            (tier.routes || []).map((r) => ui().escapeHtml(routeLabel(r))).join("<br>") +
            "</span></div></div>";
        });
      } else {
        body = '<div class="group-flat-routes">' +
          (group.routes || []).map((r) => "<code>" + ui().escapeHtml(r) + "</code>").join("") + "</div>";
      }
      html += '<article class="group-card"><div class="group-card-head"><div><div class="group-badges">' +
        badges + "</div><h3>" + ui().escapeHtml(group.id) +
        '</h3></div><div class="group-actions"><button class="secondary-button" data-group-edit="' +
        ui().escapeAttr(group.id) + '">Edit</button><button class="secondary-button group-delete" data-group-delete="' +
        ui().escapeAttr(group.id) + '"' + (group.is_default ? " disabled" : "") +
        ">Delete</button></div></div><div class=\"group-tier-list\">" + body + "</div></article>";
    }
    e.content.innerHTML = '<div class="group-grid">' + html + "</div>";
    e.content.querySelectorAll("[data-group-edit]").forEach((b) => b.addEventListener("click", () => open(b.dataset.groupEdit)));
    e.content.querySelectorAll("[data-group-delete]").forEach((b) => b.addEventListener("click", () => remove(b.dataset.groupDelete)));
  }

  function open(id = null) {
    const group = id ? state.groups.find((g) => g.id === id) : null;
    state.editingId = group?.id || null;
    e.title.textContent = group ? "Edit " + group.id : "Create model group";
    e.id.value = group?.id || "";
    e.id.disabled = Boolean(group);
    if (group?.mode === "tiered") state.tiers = group.tiers.map((t) => ({ priority: Number(t.priority), routes: [...t.routes] }));
    else if (group) state.tiers = [{ priority: 10, routes: [...group.routes] }];
    else state.tiers = [{ priority: 10, routes: [] }];
    hideError();
    renderEditor();
    e.modal.classList.remove("hidden");
    if (!group) requestAnimationFrame(() => e.id.focus());
  }

  function renderEditor() {
    const assigned = new Map();
    state.tiers.forEach((tier, i) => tier.routes.forEach((route) => assigned.set(route, i)));
    e.tiers.innerHTML = state.tiers.map((tier, i) => {
      const choices = state.routes.map((route) => {
        const checked = tier.routes.includes(route.id);
        const elsewhere = assigned.has(route.id) && assigned.get(route.id) !== i;
        return '<label class="group-route-choice' + (elsewhere ? " assigned" : "") + '">' +
          '<input type="checkbox" data-route-tier="' + i + '" data-route-id="' + ui().escapeAttr(route.id) + '"' +
          (checked ? " checked" : "") + '><span><strong>' + ui().escapeHtml(route.id) + "</strong><small>" +
          ui().escapeHtml(route.provider + " · " + route.model + " · " + route.account + (route.enabled ? "" : " · disabled")) +
          "</small></span></label>";
      }).join("");
      return '<section class="group-tier-editor"><div class="group-tier-head"><label><span>Tier priority</span>' +
        '<input class="search-input compact group-priority" type="number" min="0" max="100000" data-priority-tier="' + i +
        '" value="' + Number(tier.priority) + '"></label><button class="icon-button" data-remove-tier="' + i + '"' +
        (state.tiers.length === 1 ? " disabled" : "") + '>✕</button></div><div class="group-route-list">' +
        (choices || '<div class="group-no-routes">No configured routes</div>') + "</div></section>";
    }).join("");

    e.tiers.querySelectorAll("[data-priority-tier]").forEach((input) => input.addEventListener("input", () => {
      state.tiers[Number(input.dataset.priorityTier)].priority = Number(input.value);
    }));
    e.tiers.querySelectorAll("[data-route-id]").forEach((checkbox) => checkbox.addEventListener("change", () => {
      const i = Number(checkbox.dataset.routeTier), route = checkbox.dataset.routeId;
      if (checkbox.checked) {
        state.tiers.forEach((tier, index) => { if (index !== i) tier.routes = tier.routes.filter((x) => x !== route); });
        if (!state.tiers[i].routes.includes(route)) state.tiers[i].routes.push(route);
      } else state.tiers[i].routes = state.tiers[i].routes.filter((x) => x !== route);
      renderEditor();
    }));
    e.tiers.querySelectorAll("[data-remove-tier]").forEach((button) => button.addEventListener("click", () => {
      if (state.tiers.length > 1) { state.tiers.splice(Number(button.dataset.removeTier), 1); renderEditor(); }
    }));
  }

  function payload() {
    const id = e.id.value.trim();
    if (!/^[A-Za-z0-9._-]{1,96}$/.test(id)) throw new Error("Enter a valid group ID using letters, numbers, '.', '_' or '-'.");
    const priorities = new Set();
    for (const tier of state.tiers) {
      if (!Number.isInteger(tier.priority) || tier.priority < 0) throw new Error("Tier priorities must be non-negative integers.");
      if (priorities.has(tier.priority)) throw new Error("Each tier priority must be unique.");
      if (!tier.routes.length) throw new Error("Every tier must contain at least one route.");
      priorities.add(tier.priority);
    }
    return { id, tiers: state.tiers.map((t) => ({ priority: t.priority, routes: [...t.routes] })).sort((a,b) => a.priority-b.priority) };
  }

  async function save() {
    let p;
    try { p = payload(); } catch (error) { return showError(error.message); }
    e.save.disabled = true;
    const old = e.save.textContent; e.save.textContent = "Saving…";
    try {
      const editing = Boolean(state.editingId);
      const response = await ui().apiFetch(editing ? "/_llmgateway/model-groups/" + encodeURIComponent(state.editingId) : "/_llmgateway/model-groups", {
        method: editing ? "PUT" : "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(editing ? { tiers: p.tiers } : p),
      });
      if (!response.ok) throw new Error(ui().extractError(await response.text(), response.status));
      e.modal.classList.add("hidden");
      state.loaded = false;
      await load(true);
      window.dispatchEvent(new CustomEvent("llmgateway:models-changed"));
      ui().toast((editing ? "Updated " : "Created ") + p.id);
    } catch (error) { showError(error.message || String(error)); }
    finally { e.save.disabled = false; e.save.textContent = old; }
  }

  async function remove(id) {
    if (!confirm('Delete model group "' + id + '"?')) return;
    try {
      const response = await ui().apiFetch("/_llmgateway/model-groups/" + encodeURIComponent(id), { method: "DELETE" });
      if (!response.ok) throw new Error(ui().extractError(await response.text(), response.status));
      state.loaded = false; await load(true);
      window.dispatchEvent(new CustomEvent("llmgateway:models-changed"));
      ui().toast("Deleted " + id);
    } catch (error) { ui().toast(error.message || String(error)); }
  }

  function showError(message) { e.error.textContent = message; e.error.classList.remove("hidden"); }
  function hideError() { e.error.textContent = ""; e.error.classList.add("hidden"); }
  e.create?.addEventListener("click", () => open());
  e.refresh?.addEventListener("click", () => load(true));
  e.addTier?.addEventListener("click", () => { const max = state.tiers.reduce((m,t) => Math.max(m,t.priority || 0),0); state.tiers.push({priority:max+10,routes:[]}); renderEditor(); });
  e.save?.addEventListener("click", save);
  e.modal?.addEventListener("click", (event) => { if (event.target === e.modal) e.modal.classList.add("hidden"); });
  window.addEventListener("llmgateway:view-changed", (event) => { if (event.detail?.view === "groups") load(); });
  window.addEventListener("llmgateway:models-changed", () => { state.loaded = false; if ($("groupsView")?.classList.contains("active-view")) load(true); });
})();