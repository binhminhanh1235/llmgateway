use crate::api::{authorize, json_response, AppState};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

#[derive(Debug, Deserialize)]
pub struct ExplainRoutesRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub client_id: Option<String>,
}

pub async fn explain_routes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExplainRoutesRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let config = state.gateway.config_snapshot();
    let requested_model = request
        .model
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| config.api.default_model.clone());

    let has_task_context = request.task.is_some() || request.body.is_some();
    let mut body = request
        .body
        .unwrap_or_else(|| Value::Object(Map::new()));
    if let Some(task) = request.task.filter(|task| !task.trim().is_empty()) {
        if let Some(object) = body.as_object_mut() {
            object.insert("llmgateway_task".into(), Value::String(task));
        }
    }

    let client_policy = match request.client_id.as_deref() {
        Some(client_id) => match config.clients.get(client_id) {
            Some(policy) => Some(policy.clone()),
            None => {
                return crate::api::json_error(
                    StatusCode::BAD_REQUEST,
                    "client_policy_error",
                    &format!("unknown client policy '{client_id}'"),
                )
            }
        },
        None => None,
    };
    let effective_config = match state
        .gateway
        .effective_request_config(config.clone(), client_policy.as_ref(), &body)
    {
        Ok(config) => config,
        Err(error) => return crate::api::gateway_error(error),
    };
    let mut trace = state
        .gateway
        .router
        .explain_for_body_with_config(
            effective_config,
            &requested_model,
            has_task_context.then_some(&body),
        )
        .await;

    if let Some(policy) = client_policy.as_ref() {
        let resolved = config.resolve_model_alias(&requested_model).to_string();
        let model_allowed = policy.model_allowed(&requested_model, &resolved);
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
        rerank_after_client_policy(&mut trace);
    }

    json_response(StatusCode::OK, json!(trace), None)
}

fn push_reason(values: &mut Vec<String>, reason: &str) {
    if !values.iter().any(|value| value == reason) {
        values.push(reason.to_string());
    }
}

fn rerank_after_client_policy(trace: &mut crate::routing::RouteDecisionTrace) {
    let mut indices = trace
        .candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.eligible)
        .map(|(index, candidate)| (index, candidate.rank.unwrap_or(usize::MAX)))
        .collect::<Vec<_>>();
    indices.sort_by_key(|(_, rank)| *rank);

    trace.selected_route = None;
    for candidate in &mut trace.candidates {
        candidate.rank = None;
        candidate.selected = false;
    }
    for (rank_index, (candidate_index, _)) in indices.into_iter().enumerate() {
        let candidate = &mut trace.candidates[candidate_index];
        candidate.rank = Some(rank_index + 1);
        candidate.selected = rank_index == 0;
        if rank_index == 0 {
            trace.selected_route = Some(candidate.route_id.clone());
        }
    }
}
