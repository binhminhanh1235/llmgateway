(() => {
  const state = { groups: [], models: [], editingId: null, tiers: [], loaded: false };
  const $ = (id) => document.getElementById(id);
  const e = {
    content: $("groupsContent"),
    create: $("createModelGroupButton"),
    refresh: $("refreshModelGroupsButton"),
    modal: $("modelGroupModal"),
    title: $("modelGroupModalTitle"),
    id: $("modelGroupIdInput"),
    tiers: $("modelGroupTiers"),
    addTier: $("addModelGroupTierButton"),
    save: $("saveModelGroupButton"),
    error: $("modelGroupError"),
  };
  const ui = () => window.llmgatewayUI;

  async function load(force = false) {
    if (!ui()?.apiFetch) return;
    if (state.loaded && !force) return render();
    e.content.innerHTML = '<div class="loading-box">Loading model groups…</div>';
    try {
      const response = await ui().apiFetch("/_llmgateway/model-groups");
      if (!response.ok) {
        throw new Error(ui().extractError(await response.text(), response.status));
      }
      const body = await response.json();
      state.groups = body.data || [];
      state.models = body.models || [];
      state.loaded = true;
      render();
    } catch (error) {
      e.content.innerHTML =
        '<div class="error-box">' + ui().escapeHtml(error.message || error) + "</div>";
    }
  }

  function modelById(id) {
    return state.models.find((model) => model.id === id);
  }

  function modelName(id) {
    return modelById(id)?.display_name || id;
  }

  function modelMeta(id) {
    const model = modelById(id);
    if (!model) return "currently inactive";
    const accounts = (model.active_accounts || []).join(", ");
    return model.provider + " · " + model.external_id + (accounts ? " · " + accounts : "");
  }

  function renderMembers(models) {
    if (!(models || []).length) return "No model members";
    return models.map((id, index) =>
      '<span class="group-summary-member"><b>' + (index + 1) + ".</b><span><strong>" +
      ui().escapeHtml(modelName(id)) + "</strong><small>" +
      ui().escapeHtml(modelMeta(id)) + "</small></span></span>"
    ).join("");
  }

  function render() {
    if (!state.groups.length) {
      e.content.innerHTML =
        '<div class="loading-box">No model groups yet. Create one to define ordered fallback.</div>';
      return;
    }

    let html = "";
    for (const group of state.groups) {
      const defaultBadge = group.is_default
        ? '<span class="badge available">Default</span>'
        : "";
      const modeLabel = group.mode === "model-tiered"
        ? "Ordered models"
        : group.mode === "route-tiered"
          ? "Legacy route tiers"
          : "Legacy flat";
      const badges = defaultBadge + '<span class="badge">' + modeLabel + "</span>";

      let body = "";
      if ((group.tiers || []).length) {
        (group.tiers || []).forEach((tier, index) => {
          body += '<div class="group-tier-summary"><div class="group-tier-number">' +
            (index + 1) + '</div><div class="group-tier-summary-body"><strong>Tier priority ' +
            Number(tier.priority) + '</strong><div class="group-summary-members">' +
            renderMembers(tier.models || []) + "</div></div></div>";
        });
      } else {
        body = '<div class="group-summary-members group-flat-models">' +
          renderMembers(group.models || []) + "</div>";
      }

      html += '<article class="group-card"><div class="group-card-head"><div><div class="group-badges">' +
        badges + "</div><h3>" + ui().escapeHtml(group.id) +
        '</h3></div><div class="group-actions"><button class="secondary-button" data-group-edit="' +
        ui().escapeAttr(group.id) +
        '">Edit</button><button class="secondary-button group-delete" data-group-delete="' +
        ui().escapeAttr(group.id) + '"' + (group.is_default ? " disabled" : "") +
        '>Delete</button></div></div><div class="group-tier-list">' + body + "</div></article>";
    }

    e.content.innerHTML = '<div class="group-grid">' + html + "</div>";
    e.content.querySelectorAll("[data-group-edit]").forEach((button) =>
      button.addEventListener("click", () => open(button.dataset.groupEdit)));
    e.content.querySelectorAll("[data-group-delete]").forEach((button) =>
      button.addEventListener("click", () => remove(button.dataset.groupDelete)));
  }

  function activeOnly(ids) {
    const active = new Set(state.models.map((model) => model.id));
    return (ids || []).filter((id) => active.has(id));
  }

  function open(id = null) {
    const group = id ? state.groups.find((candidate) => candidate.id === id) : null;
    state.editingId = group?.id || null;
    e.title.textContent = group ? "Edit " + group.id : "Create model group";
    e.id.value = group?.id || "";
    e.id.disabled = Boolean(group);

    if (group && (group.tiers || []).length) {
      state.tiers = group.tiers.map((tier) => ({
        priority: Number(tier.priority),
        models: activeOnly(tier.models),
      }));
    } else if (group) {
      state.tiers = [{ priority: 10, models: activeOnly(group.models) }];
    } else {
      state.tiers = [{ priority: 10, models: [] }];
    }

    hideError();
    renderEditor();
    e.modal.classList.remove("hidden");
    if (!group) requestAnimationFrame(() => e.id.focus());
  }

  function selectedOrderHtml(tier, tierIndex) {
    if (!tier.models.length) {
      return '<div class="group-order-empty">Select models below. Their fallback order will appear here.</div>';
    }

    return tier.models.map((modelId, modelIndex) => {
      const first = modelIndex === 0;
      const last = modelIndex === tier.models.length - 1;
      return '<div class="group-order-model"><div class="group-order-rank">' +
        (modelIndex + 1) + '</div><div class="group-order-copy"><strong>' +
        ui().escapeHtml(modelName(modelId)) + "</strong><small>" +
        ui().escapeHtml(modelMeta(modelId)) +
        '</small></div><div class="group-order-actions">' +
        '<button class="icon-button" type="button" title="Move up" data-model-up="' +
        tierIndex + ":" + modelIndex + '"' + (first ? " disabled" : "") + ">↑</button>" +
        '<button class="icon-button" type="button" title="Move down" data-model-down="' +
        tierIndex + ":" + modelIndex + '"' + (last ? " disabled" : "") + ">↓</button>" +
        '<button class="icon-button" type="button" title="Remove model" data-model-remove="' +
        tierIndex + ":" + modelIndex + '">✕</button></div></div>';
    }).join("");
  }

  function availableModelsHtml(tierIndex, assigned) {
    if (!state.models.length) {
      return '<div class="group-no-routes">No enabled and active models are currently available.</div>';
    }

    return state.models.map((model) => {
      const selectedHere = state.tiers[tierIndex].models.includes(model.id);
      const assignedTier = assigned.get(model.id);
      const assignedElsewhere = assignedTier !== undefined && assignedTier !== tierIndex;
      const capabilities = (model.capabilities || []).slice(0, 5).join(" · ");
      const accounts = (model.active_accounts || []).join(", ");
      const detail = model.provider + " · " + model.external_id +
        (accounts ? " · " + accounts : "") +
        (capabilities ? " · " + capabilities : "");

      return '<label class="group-route-choice group-model-choice' +
        (assignedElsewhere ? " assigned" : "") + '">' +
        '<input type="checkbox" data-model-tier="' + tierIndex +
        '" data-model-id="' + ui().escapeAttr(model.id) + '"' +
        (selectedHere ? " checked" : "") +
        (assignedElsewhere ? " disabled" : "") +
        '><span><strong>' + ui().escapeHtml(model.display_name) +
        "</strong><small>" + ui().escapeHtml(detail) + "</small></span></label>";
    }).join("");
  }

  function renderEditor() {
    const assigned = new Map();
    state.tiers.forEach((tier, tierIndex) =>
      tier.models.forEach((modelId) => assigned.set(modelId, tierIndex)));

    e.tiers.innerHTML = state.tiers.map((tier, tierIndex) => {
      return '<section class="group-tier-editor">' +
        '<div class="group-tier-head"><label><span>Tier priority</span>' +
        '<input class="search-input compact group-priority" type="number" min="0" max="100000" ' +
        'data-priority-tier="' + tierIndex + '" value="' + Number(tier.priority) +
        '"></label><button class="icon-button" type="button" data-remove-tier="' + tierIndex + '"' +
        (state.tiers.length === 1 ? " disabled" : "") + '>✕</button></div>' +
        '<div class="group-order-section"><div class="group-order-heading"><strong>Fallback order</strong>' +
        '<span>Models are tried top to bottom. Use ↑ ↓ to reorder.</span></div>' +
        '<div class="group-order-list">' + selectedOrderHtml(tier, tierIndex) + "</div></div>" +
        '<div class="group-active-model-note">Available models · only enabled models on active accounts are selectable.</div>' +
        '<div class="group-route-list">' + availableModelsHtml(tierIndex, assigned) + "</div></section>";
    }).join("");

    e.tiers.querySelectorAll("[data-priority-tier]").forEach((input) =>
      input.addEventListener("input", () => {
        state.tiers[Number(input.dataset.priorityTier)].priority = Number(input.value);
      }));

    e.tiers.querySelectorAll("[data-model-id]").forEach((checkbox) =>
      checkbox.addEventListener("change", () => {
        const tierIndex = Number(checkbox.dataset.modelTier);
        const modelId = checkbox.dataset.modelId;
        if (checkbox.checked) {
          if (!state.tiers[tierIndex].models.includes(modelId)) {
            state.tiers[tierIndex].models.push(modelId);
          }
        } else {
          state.tiers[tierIndex].models =
            state.tiers[tierIndex].models.filter((candidate) => candidate !== modelId);
        }
        renderEditor();
      }));

    e.tiers.querySelectorAll("[data-model-up]").forEach((button) =>
      button.addEventListener("click", () => moveModel(button.dataset.modelUp, -1)));
    e.tiers.querySelectorAll("[data-model-down]").forEach((button) =>
      button.addEventListener("click", () => moveModel(button.dataset.modelDown, 1)));
    e.tiers.querySelectorAll("[data-model-remove]").forEach((button) =>
      button.addEventListener("click", () => removeModel(button.dataset.modelRemove)));

    e.tiers.querySelectorAll("[data-remove-tier]").forEach((button) =>
      button.addEventListener("click", () => {
        if (state.tiers.length > 1) {
          state.tiers.splice(Number(button.dataset.removeTier), 1);
          renderEditor();
        }
      }));
  }

  function parsePosition(value) {
    const [tierIndex, modelIndex] = String(value).split(":").map(Number);
    return { tierIndex, modelIndex };
  }

  function moveModel(value, delta) {
    const { tierIndex, modelIndex } = parsePosition(value);
    const models = state.tiers[tierIndex].models;
    const target = modelIndex + delta;
    if (target < 0 || target >= models.length) return;
    [models[modelIndex], models[target]] = [models[target], models[modelIndex]];
    renderEditor();
  }

  function removeModel(value) {
    const { tierIndex, modelIndex } = parsePosition(value);
    state.tiers[tierIndex].models.splice(modelIndex, 1);
    renderEditor();
  }

  function payload() {
    const id = e.id.value.trim();
    if (!/^[A-Za-z0-9._-]{1,96}$/.test(id)) {
      throw new Error("Enter a valid group ID using letters, numbers, '.', '_' or '-'.");
    }

    const priorities = new Set();
    for (const tier of state.tiers) {
      if (!Number.isInteger(tier.priority) || tier.priority < 0) {
        throw new Error("Tier priorities must be non-negative integers.");
      }
      if (priorities.has(tier.priority)) {
        throw new Error("Each tier priority must be unique.");
      }
      if (!tier.models.length) {
        throw new Error("Every tier must contain at least one enabled model.");
      }
      priorities.add(tier.priority);
    }

    return {
      id,
      tiers: state.tiers
        .map((tier) => ({ priority: tier.priority, models: [...tier.models] }))
        .sort((left, right) => left.priority - right.priority),
    };
  }

  async function save() {
    let body;
    try {
      body = payload();
    } catch (error) {
      showError(error.message);
      return;
    }

    e.save.disabled = true;
    const previous = e.save.textContent;
    e.save.textContent = "Saving…";
    try {
      const editing = Boolean(state.editingId);
      const response = await ui().apiFetch(
        editing
          ? "/_llmgateway/model-groups/" + encodeURIComponent(state.editingId)
          : "/_llmgateway/model-groups",
        {
          method: editing ? "PUT" : "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(editing ? { tiers: body.tiers } : body),
        }
      );
      if (!response.ok) {
        throw new Error(ui().extractError(await response.text(), response.status));
      }

      e.modal.classList.add("hidden");
      state.loaded = false;
      await load(true);
      window.dispatchEvent(new CustomEvent("llmgateway:models-changed"));
      ui().toast((editing ? "Updated " : "Created ") + body.id);
    } catch (error) {
      showError(error.message || String(error));
    } finally {
      e.save.disabled = false;
      e.save.textContent = previous;
    }
  }

  async function remove(id) {
    if (!confirm('Delete model group "' + id + '"?')) return;
    try {
      const response = await ui().apiFetch(
        "/_llmgateway/model-groups/" + encodeURIComponent(id),
        { method: "DELETE" }
      );
      if (!response.ok) {
        throw new Error(ui().extractError(await response.text(), response.status));
      }
      state.loaded = false;
      await load(true);
      window.dispatchEvent(new CustomEvent("llmgateway:models-changed"));
      ui().toast("Deleted " + id);
    } catch (error) {
      ui().toast(error.message || String(error));
    }
  }

  function showError(message) {
    e.error.textContent = message;
    e.error.classList.remove("hidden");
  }

  function hideError() {
    e.error.textContent = "";
    e.error.classList.add("hidden");
  }

  e.create?.addEventListener("click", () => open());
  e.refresh?.addEventListener("click", () => load(true));
  e.addTier?.addEventListener("click", () => {
    const max = state.tiers.reduce(
      (value, tier) => Math.max(value, Number(tier.priority) || 0),
      0
    );
    state.tiers.push({ priority: max + 10, models: [] });
    renderEditor();
  });
  e.save?.addEventListener("click", save);
  e.modal?.addEventListener("click", (event) => {
    if (event.target === e.modal) e.modal.classList.add("hidden");
  });

  window.addEventListener("llmgateway:view-changed", (event) => {
    if (event.detail?.view === "groups") load();
  });
  window.addEventListener("llmgateway:models-changed", () => {
    state.loaded = false;
    if ($("groupsView")?.classList.contains("active-view")) load(true);
  });
})();