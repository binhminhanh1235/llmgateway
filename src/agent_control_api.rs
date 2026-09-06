use crate::{
    api::{
        authorize_client, client_policy_error, gateway_error, json_error, json_response, AppState,
    },
    client_policy::ClientAccess,
    config::ClientPolicyConfig,
    routing::RouteDecisionTrace,
    routing_api::{push_reason, rerank_after_client_policy},
};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentRequirements {
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub min_context_window: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentRouteRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub requirements: AgentRequirements,
}

pub async fn agent_capabilities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };

    let config = state.gateway.config_snapshot();
    let effective_config = state
        .gateway
        .effective_client_config(config.clone(), access.policy());
    let physical = match state.catalog.selectable_models().await {
        Ok(models) => models,
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "agent_capability_catalog_error",
                &error.to_string(),
            )
        }
    };

    let mut models = Vec::new();
    let mut global_capabilities = BTreeSet::new();

    for (id, group) in &config.virtual_models {
        if !group.enabled || !model_allowed(access.policy(), id, id) {
            continue;
        }
        let mut trace = state
            .gateway
            .router
            .explain_for_body_with_config(effective_config.clone(), id, None)
            .await;
        apply_policy(&config, access.policy(), id, &mut trace);

        let mut capabilities = BTreeSet::new();
        let mut context_window = None;
        for candidate in &trace.candidates {
            for capability in &candidate.capabilities {
                capabilities.insert(capability.clone());
                global_capabilities.insert(capability.clone());
            }
            context_window = max_context(context_window, candidate.context_window);
        }

        models.push(json!({
            "id": id,
            "kind": "virtual",
            "capabilities": capabilities.into_iter().collect::<Vec<_>>(),
            "max_known_context_window": context_window,
            "eligible_routes": trace.candidates.iter().filter(|candidate| candidate.eligible).count(),
            "total_routes": trace.candidates.len()
        }));
    }

    for model in physical {
        if !model_allowed(access.policy(), &model.id, &model.id) {
            continue;
        }
        for capability in &model.capabilities {
            global_capabilities.insert(normalize_capability(capability));
        }
        models.push(json!({
            "id": model.id,
            "kind": "physical",
            "provider": model.provider,
            "display_name": model.display_name,
            "capabilities": normalized_capabilities(&model.capabilities),
            "context_window": model.context_window,
            "available_accounts": model.accounts.iter().filter(|binding| {
                binding.enabled && matches!(binding.availability.as_str(), "available" | "unknown")
            }).count()
        }));
    }

    models.sort_by(|left, right| {
        let left_kind = left.get("kind").and_then(Value::as_str).unwrap_or("");
        let right_kind = right.get("kind").and_then(Value::as_str).unwrap_or("");
        let left_id = left.get("id").and_then(Value::as_str).unwrap_or("");
        let right_id = right.get("id").and_then(Value::as_str).unwrap_or("");
        (kind_rank(left_kind), left_id).cmp(&(kind_rank(right_kind), right_id))
    });

    json_response(
        StatusCode::OK,
        json!({
            "object": "llmgateway.agent.capabilities",
            "client_id": access.client_id(),
            "policy_applied": access.policy().is_some(),
            "default_model": config.api.default_model,
            "capabilities": global_capabilities.into_iter().collect::<Vec<_>>(),
            "models": models
        }),
        None,
    )
}

pub async fn agent_resolve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AgentRouteRequest>,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };

    match resolve(&state, &access, request).await {
        Ok(value) => json_response(StatusCode::OK, value, None),
        Err(response) => response,
    }
}

pub async fn agent_diagnostics(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AgentRouteRequest>,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };

    let resolved = match resolve(&state, &access, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };

    let status = resolved
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unresolved");
    let blocking = resolved
        .get("blocking_reasons")
        .cloned()
        .unwrap_or_else(|| json!({}));

    json_response(
        StatusCode::OK,
        json!({
            "object": "llmgateway.agent.diagnostics",
            "status": if status == "resolved" { "ready" } else { "blocked" },
            "gateway": "ok",
            "client_id": access.client_id(),
            "policy_applied": access.policy().is_some(),
            "resolution": resolved,
            "blocking_reasons": blocking,
            "recommended_action": recommended_action(&blocking)
        }),
        None,
    )
}

async fn resolve(
    state: &AppState,
    access: &ClientAccess,
    request: AgentRouteRequest,
) -> Result<Value, Response<Body>> {
    let config = state.gateway.config_snapshot();
    let requested_model = request
        .model
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| config.api.default_model.clone());

    if let Err(error) = state
        .client_policies
        .enforce_model(access, &requested_model)
    {
        return Err(client_policy_error(error));
    }

    if request
        .requirements
        .min_context_window
        .is_some_and(|value| value <= 0)
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "agent_requirement_error",
            "min_context_window must be greater than zero",
        ));
    }

    let requirements = normalized_requirements(request.requirements);
    let mut body = request.body.unwrap_or_else(|| Value::Object(Map::new()));
    let Some(object) = body.as_object_mut() else {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "agent_request_error",
            "body must be a JSON object",
        ));
    };
    if let Some(task) = request.task.filter(|task| !task.trim().is_empty()) {
        object.insert("llmgateway_task".into(), Value::String(task));
    }
    object.insert(
        "llmgateway_requirements".into(),
        json!({
            "capabilities": requirements.capabilities,
            "min_context_window": requirements.min_context_window
        }),
    );

    let effective_config =
        match state
            .gateway
            .effective_request_config(config.clone(), access.policy(), &body)
        {
            Ok(config) => config,
            Err(error) => return Err(gateway_error(error)),
        };

    let mut trace = state
        .gateway
        .router
        .explain_for_body_with_config(effective_config, &requested_model, Some(&body))
        .await;
    apply_policy(&config, access.policy(), &requested_model, &mut trace);

    let selected = trace
        .candidates
        .iter()
        .find(|candidate| candidate.selected)
        .map(|candidate| {
            json!({
                "route_id": candidate.route_id,
                "account": candidate.account,
                "model": candidate.model,
                "transport": candidate.transport,
                "capabilities": candidate.capabilities,
                "context_window": candidate.context_window,
                "warnings": candidate.warnings,
                "rank": candidate.rank
            })
        });

    let mut blocking_reasons = BTreeMap::<String, usize>::new();
    for candidate in trace
        .candidates
        .iter()
        .filter(|candidate| !candidate.eligible)
    {
        for reason in &candidate.exclusion_reasons {
            *blocking_reasons.entry(reason.clone()).or_default() += 1;
        }
    }

    let candidates = trace
        .candidates
        .iter()
        .map(|candidate| {
            json!({
                "route_id": candidate.route_id,
                "account": candidate.account,
                "model": candidate.model,
                "transport": candidate.transport,
                "eligible": candidate.eligible,
                "selected": candidate.selected,
                "rank": candidate.rank,
                "capabilities": candidate.capabilities,
                "context_window": candidate.context_window,
                "missing_required_capabilities": candidate.missing_required_capabilities,
                "exclusion_reasons": candidate.exclusion_reasons,
                "warnings": candidate.warnings
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "object": "llmgateway.agent.resolve",
        "status": if selected.is_some() { "resolved" } else { "unresolved" },
        "client_id": access.client_id(),
        "policy_applied": access.policy().is_some(),
        "requested_model": trace.requested_model,
        "resolved_model": trace.resolved_model,
        "requirements": trace.requirements,
        "task": trace.task,
        "selected": selected,
        "eligible_routes": trace.candidates.iter().filter(|candidate| candidate.eligible).count(),
        "total_routes": trace.candidates.len(),
        "blocking_reasons": blocking_reasons,
        "candidates": candidates
    }))
}

fn apply_policy(
    config: &crate::config::AppConfig,
    policy: Option<&ClientPolicyConfig>,
    requested_model: &str,
    trace: &mut RouteDecisionTrace,
) {
    let Some(policy) = policy else {
        return;
    };

    let resolved = config.resolve_model_alias(requested_model).to_string();
    let model_allowed = policy.model_allowed(requested_model, &resolved);
    for candidate in &mut trace.candidates {
        let mut denied = false;
        if !model_allowed {
            push_reason(
                &mut candidate.exclusion_reasons,
                "client_policy_model_forbidden",
            );
            denied = true;
        }
        if !policy.route_allowed(&candidate.route_id) {
            push_reason(
                &mut candidate.exclusion_reasons,
                "client_policy_route_forbidden",
            );
            denied = true;
        }
        if denied {
            candidate.eligible = false;
            candidate.final_score = None;
            candidate.rank = None;
            candidate.selected = false;
        }
    }
    rerank_after_client_policy(trace);
}

fn normalized_requirements(mut requirements: AgentRequirements) -> AgentRequirements {
    requirements.capabilities = normalized_capabilities(&requirements.capabilities);
    requirements
}

fn normalized_capabilities(values: &[String]) -> Vec<String> {
    let mut out = values
        .iter()
        .map(|value| normalize_capability(value))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    out.sort();
    out.dedup();
    out
}

fn normalize_capability(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('_', "-")
}

fn model_allowed(policy: Option<&ClientPolicyConfig>, requested: &str, resolved: &str) -> bool {
    policy.is_none_or(|policy| policy.model_allowed(requested, resolved))
}

fn max_context(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "virtual" => 0,
        "physical" => 1,
        _ => 2,
    }
}

fn recommended_action(blocking: &Value) -> &'static str {
    let Some(reasons) = blocking.as_object() else {
        return "inspect_route_evidence";
    };

    if reasons.contains_key("client_policy_model_forbidden")
        || reasons.contains_key("client_policy_route_forbidden")
    {
        "use_client_allowed_model_or_route"
    } else if reasons.contains_key("required_capability_missing")
        || reasons.contains_key("minimum_context_window_not_met")
        || reasons.contains_key("context_window_unknown")
    {
        "relax_requirements_or_enable_capable_model"
    } else if reasons
        .keys()
        .any(|reason| reason.contains("auth") || reason.contains("login"))
    {
        "reverify_provider_authentication"
    } else if reasons.contains_key("quota_blocked") || reasons.contains_key("route_cooldown") {
        "wait_for_quota_or_cooldown_or_use_fallback"
    } else if reasons.is_empty() {
        "none"
    } else {
        "inspect_account_model_and_route_health"
    }
}

#[cfg(test)]
mod tests {
    use super::{normalized_capabilities, recommended_action};
    use serde_json::json;

    #[test]
    fn capability_names_are_normalized_and_deduplicated() {
        assert_eq!(
            normalized_capabilities(&["Coding".into(), "long_context".into(), "coding".into()]),
            vec!["coding", "long-context"]
        );
    }

    #[test]
    fn diagnostics_maps_capability_blockers_to_action() {
        let reasons = json!({"required_capability_missing": 2});
        assert_eq!(
            recommended_action(&reasons),
            "relax_requirements_or_enable_capable_model"
        );
    }
}
