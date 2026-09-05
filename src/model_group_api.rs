use crate::{
    api::{authorize, json_error, json_response, AppState},
    config::{AppConfig, VirtualModelConfig},
};
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, Response, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table, Value};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelGroupTierInput {
    pub priority: i32,
    pub routes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateModelGroupRequest {
    pub id: String,
    pub tiers: Vec<ModelGroupTierInput>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateModelGroupRequest {
    pub tiers: Vec<ModelGroupTierInput>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelGroupTierView {
    pub priority: i32,
    pub routes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelGroupView {
    pub id: String,
    pub mode: &'static str,
    pub is_default: bool,
    pub routes: Vec<String>,
    pub tiers: Vec<ModelGroupTierView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelGroupRouteView {
    pub id: String,
    pub account: String,
    pub provider: String,
    pub model: String,
    pub priority: i32,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelGroupMutationResult {
    pub group: ModelGroupView,
    pub config_path: String,
    pub backup_path: Option<String>,
    pub restart_required: bool,
}

#[derive(Debug, Error)]
pub enum ModelGroupError {
    #[error("model group storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("model group TOML error: {0}")]
    TomlEdit(#[from] toml_edit::TomlError),
    #[error("model group produced invalid gateway configuration: {0}")]
    InvalidGatewayConfig(#[from] crate::config::ConfigError),
    #[error("model group '{0}' already exists")]
    Conflict(String),
    #[error("model group '{0}' was not found")]
    NotFound(String),
    #[error("invalid model group: {0}")]
    Invalid(String),
}

pub async fn list_model_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let config = state.gateway.config_snapshot();
    json_response(
        StatusCode::OK,
        json!({
            "data": group_views(config.as_ref()),
            "routes": route_views(config.as_ref()),
            "default_model": config.api.default_model
        }),
        None,
    )
}

pub async fn create_model_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateModelGroupRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let config_path = gateway_config_path();
    match apply_model_group(&config_path, &body.id, body.tiers, false) {
        Ok(backup_path) => mutation_response(
            &state,
            &config_path,
            body.id.trim(),
            backup_path,
            StatusCode::CREATED,
        ),
        Err(error) => model_group_error_response(error),
    }
}

pub async fn update_model_group(
    State(state): State<AppState>,
    AxumPath(group_id): AxumPath<String>,
    headers: HeaderMap,
    Json(body): Json<UpdateModelGroupRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let config_path = gateway_config_path();
    match apply_model_group(&config_path, &group_id, body.tiers, true) {
        Ok(backup_path) => mutation_response(
            &state,
            &config_path,
            group_id.trim(),
            backup_path,
            StatusCode::OK,
        ),
        Err(error) => model_group_error_response(error),
    }
}

pub async fn delete_model_group(
    State(state): State<AppState>,
    AxumPath(group_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    let config_path = gateway_config_path();
    match remove_model_group(&config_path, &group_id) {
        Ok(backup_path) => match activate_model_groups(&state, &config_path) {
            Ok(_) => json_response(
                StatusCode::OK,
                json!({
                    "id": group_id,
                    "deleted": true,
                    "config_path": config_path,
                    "backup_path": backup_path.map(|path| path.display().to_string()),
                    "restart_required": false
                }),
                None,
            ),
            Err(error) => model_group_error_response(error),
        },
        Err(error) => model_group_error_response(error),
    }
}

fn mutation_response(
    state: &AppState,
    config_path: &str,
    group_id: &str,
    backup_path: Option<PathBuf>,
    status: StatusCode,
) -> Response<Body> {
    match activate_model_groups(state, config_path) {
        Ok(config) => {
            let Some(group) = config.virtual_models.get(group_id) else {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "model_group_activation_error",
                    "model group was not present after hot activation",
                );
            };
            json_response(
                status,
                json!(ModelGroupMutationResult {
                    group: group_view(group_id, group, config.api.default_model == group_id),
                    config_path: config_path.to_string(),
                    backup_path: backup_path.map(|path| path.display().to_string()),
                    restart_required: false,
                }),
                None,
            )
        }
        Err(error) => model_group_error_response(error),
    }
}

fn gateway_config_path() -> String {
    env::var("LLMGATEWAY_CONFIG").unwrap_or_else(|_| "config/llmgateway.toml".into())
}

fn activate_model_groups(
    state: &AppState,
    config_path: &str,
) -> Result<Arc<AppConfig>, ModelGroupError> {
    let config = Arc::new(AppConfig::load(config_path)?);
    state.gateway.live_config.replace(config.clone());
    Ok(config)
}

fn group_views(config: &AppConfig) -> Vec<ModelGroupView> {
    let mut groups = config
        .virtual_models
        .iter()
        .map(|(id, group)| group_view(id, group, config.api.default_model == *id))
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| {
        right
            .is_default
            .cmp(&left.is_default)
            .then_with(|| left.id.cmp(&right.id))
    });
    groups
}

fn group_view(id: &str, group: &VirtualModelConfig, is_default: bool) -> ModelGroupView {
    let mut tiers = group
        .tiers
        .iter()
        .map(|tier| ModelGroupTierView {
            priority: tier.priority,
            routes: tier.routes.clone(),
        })
        .collect::<Vec<_>>();
    tiers.sort_by_key(|tier| tier.priority);
    ModelGroupView {
        id: id.to_string(),
        mode: if group.is_tiered() { "tiered" } else { "flat" },
        is_default,
        routes: group.routes.clone(),
        tiers,
    }
}

fn route_views(config: &AppConfig) -> Vec<ModelGroupRouteView> {
    let mut routes = config
        .routes
        .iter()
        .map(|route| ModelGroupRouteView {
            id: route.id.clone(),
            account: route.account.clone(),
            provider: config
                .account(&route.account)
                .map(|account| account.provider.clone())
                .unwrap_or_else(|| "unknown".into()),
            model: route.model.clone(),
            priority: route.priority,
            enabled: route.enabled,
        })
        .collect::<Vec<_>>();
    routes.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.account.cmp(&right.account))
            .then_with(|| left.id.cmp(&right.id))
    });
    routes
}

pub fn apply_model_group(
    path: impl AsRef<Path>,
    group_id: &str,
    tiers: Vec<ModelGroupTierInput>,
    update_existing: bool,
) -> Result<Option<PathBuf>, ModelGroupError> {
    let group_id = group_id.trim();
    validate_group_id(group_id)?;
    let path = path.as_ref();
    let raw = fs::read_to_string(path)?;
    let current = AppConfig::parse(&raw)?;

    match (current.virtual_models.contains_key(group_id), update_existing) {
        (true, false) => return Err(ModelGroupError::Conflict(group_id.to_string())),
        (false, true) => return Err(ModelGroupError::NotFound(group_id.to_string())),
        _ => {}
    }

    let tiers = validate_tiers(&current, tiers)?;
    let mut doc = raw.parse::<DocumentMut>()?;
    let root_key = model_group_root_key(&doc);
    let groups = ensure_table(doc.as_table_mut(), root_key)?;
    let mut group = Table::new();
    let mut tier_tables = ArrayOfTables::new();
    for tier in tiers {
        let mut table = Table::new();
        table["priority"] = value(i64::from(tier.priority));
        table["routes"] = Item::Value(Value::Array(string_array(
            tier.routes.iter().map(String::as_str),
        )));
        tier_tables.push(table);
    }
    group["tiers"] = Item::ArrayOfTables(tier_tables);
    groups.insert(group_id, Item::Table(group));

    let rendered = doc.to_string();
    AppConfig::parse(&rendered)?;
    write_validated_config(path, &rendered)
}

pub fn remove_model_group(
    path: impl AsRef<Path>,
    group_id: &str,
) -> Result<Option<PathBuf>, ModelGroupError> {
    let group_id = group_id.trim();
    validate_group_id(group_id)?;
    let path = path.as_ref();
    let raw = fs::read_to_string(path)?;
    let current = AppConfig::parse(&raw)?;
    if !current.virtual_models.contains_key(group_id) {
        return Err(ModelGroupError::NotFound(group_id.to_string()));
    }
    if current.api.default_model == group_id {
        return Err(ModelGroupError::Invalid(format!(
            "cannot delete default model group '{group_id}'"
        )));
    }
    if let Some(alias) = current.aliases.iter().find(|alias| alias.target == group_id) {
        return Err(ModelGroupError::Invalid(format!(
            "cannot delete model group '{group_id}' while alias '{}' targets it",
            alias.pattern
        )));
    }

    let mut doc = raw.parse::<DocumentMut>()?;
    let root_key = model_group_root_key(&doc);
    let groups = doc
        .get_mut(root_key)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| ModelGroupError::NotFound(group_id.to_string()))?;
    if groups.remove(group_id).is_none() {
        return Err(ModelGroupError::NotFound(group_id.to_string()));
    }

    let rendered = doc.to_string();
    AppConfig::parse(&rendered)?;
    write_validated_config(path, &rendered)
}

fn validate_group_id(value: &str) -> Result<(), ModelGroupError> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err(ModelGroupError::Invalid(
            "group id must be 1-96 characters using only letters, numbers, '.', '_' or '-'".into(),
        ));
    }
    Ok(())
}

fn validate_tiers(
    config: &AppConfig,
    mut tiers: Vec<ModelGroupTierInput>,
) -> Result<Vec<ModelGroupTierInput>, ModelGroupError> {
    if tiers.is_empty() {
        return Err(ModelGroupError::Invalid(
            "a model group must contain at least one tier".into(),
        ));
    }
    let mut priorities = HashSet::new();
    let mut routes = HashSet::new();
    for tier in &tiers {
        if tier.priority < 0 {
            return Err(ModelGroupError::Invalid(
                "tier priority must be non-negative".into(),
            ));
        }
        if !priorities.insert(tier.priority) {
            return Err(ModelGroupError::Invalid(format!(
                "tier priority {} is duplicated",
                tier.priority
            )));
        }
        if tier.routes.is_empty() {
            return Err(ModelGroupError::Invalid(format!(
                "tier {} must contain at least one route",
                tier.priority
            )));
        }
        for route_id in &tier.routes {
            if config.route(route_id).is_none() {
                return Err(ModelGroupError::Invalid(format!(
                    "tier {} references unknown route '{}'",
                    tier.priority, route_id
                )));
            }
            if !routes.insert(route_id.as_str()) {
                return Err(ModelGroupError::Invalid(format!(
                    "route '{}' is assigned to more than one tier",
                    route_id
                )));
            }
        }
    }
    tiers.sort_by_key(|tier| tier.priority);
    Ok(tiers)
}

fn model_group_root_key(doc: &DocumentMut) -> &'static str {
    if doc.as_table().contains_key("virtual_models") {
        "virtual_models"
    } else if doc.as_table().contains_key("model_groups") {
        "model_groups"
    } else {
        "virtual_models"
    }
}

fn ensure_table<'a>(
    parent: &'a mut Table,
    key: &str,
) -> Result<&'a mut Table, ModelGroupError> {
    if !parent.contains_key(key) {
        parent.insert(key, Item::Table(Table::new()));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| {
            ModelGroupError::Invalid(format!(
                "configuration key '{key}' must be a TOML table"
            ))
        })
}

fn string_array<'a>(values: impl IntoIterator<Item = &'a str>) -> Array {
    let mut array = Array::new();
    for value in values {
        array.push(value);
    }
    array
}

fn write_validated_config(
    path: &Path,
    rendered: &str,
) -> Result<Option<PathBuf>, ModelGroupError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let backup = if path.exists() {
        let backup = path.with_extension(format!(
            "{}.bak",
            path.extension()
                .and_then(|value| value.to_str())
                .unwrap_or("toml")
        ));
        fs::copy(path, &backup)?;
        Some(backup)
    } else {
        None
    };
    let temp = parent.join(format!(
        ".llmgateway-model-groups-{}.tmp",
        Uuid::new_v4().simple()
    ));
    fs::write(&temp, rendered)?;
    if let Err(error) = fs::rename(&temp, path) {
        if error.kind() == std::io::ErrorKind::AlreadyExists
            || error.kind() == std::io::ErrorKind::PermissionDenied
        {
            fs::copy(&temp, path)?;
            let _ = fs::remove_file(&temp);
        } else {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
    }
    Ok(backup)
}

fn model_group_error_response(error: ModelGroupError) -> Response<Body> {
    match error {
        ModelGroupError::Conflict(group_id) => json_error(
            StatusCode::CONFLICT,
            "model_group_conflict",
            &format!("model group '{group_id}' already exists"),
        ),
        ModelGroupError::NotFound(group_id) => json_error(
            StatusCode::NOT_FOUND,
            "model_group_not_found",
            &format!("model group '{group_id}' was not found"),
        ),
        ModelGroupError::Invalid(message) => json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "model_group_invalid",
            &message,
        ),
        other => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "model_group_error",
            &other.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config() -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "llmgateway-model-groups-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("llmgateway.toml");
        fs::write(
            &path,
            r#"[server]
host = "127.0.0.1"
port = 7331
[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"
[[providers]]
id = "fake"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
[[accounts]]
id = "a"
provider = "fake"
api_key_env = "A_KEY"
enabled = true
discover_models = false
[[routes]]
id = "route-a"
account = "a"
model = "model-a"
priority = 100
enabled = true
[[routes]]
id = "route-b"
account = "a"
model = "model-b"
priority = 1
enabled = true
[virtual_models.llmgateway-auto]
routes = ["route-a", "route-b"]
"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn creates_updates_and_deletes_tiered_group() {
        let path = temp_config();
        apply_model_group(
            &path,
            "my-group",
            vec![
                ModelGroupTierInput {
                    priority: 20,
                    routes: vec!["route-b".into()],
                },
                ModelGroupTierInput {
                    priority: 10,
                    routes: vec!["route-a".into()],
                },
            ],
            false,
        )
        .unwrap();
        let parsed = AppConfig::load(&path).unwrap();
        assert_eq!(
            parsed.virtual_models["my-group"].route_ids(),
            vec!["route-a", "route-b"]
        );
        assert_eq!(
            parsed.virtual_models["my-group"].tier_priority("route-a"),
            Some(10)
        );

        apply_model_group(
            &path,
            "my-group",
            vec![ModelGroupTierInput {
                priority: 5,
                routes: vec!["route-b".into()],
            }],
            true,
        )
        .unwrap();
        assert_eq!(
            AppConfig::load(&path).unwrap().virtual_models["my-group"].route_ids(),
            vec!["route-b"]
        );

        remove_model_group(&path, "my-group").unwrap();
        assert!(
            !AppConfig::load(&path)
                .unwrap()
                .virtual_models
                .contains_key("my-group")
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rejects_duplicate_route_across_tiers() {
        let path = temp_config();
        let error = apply_model_group(
            &path,
            "bad-group",
            vec![
                ModelGroupTierInput {
                    priority: 10,
                    routes: vec!["route-a".into()],
                },
                ModelGroupTierInput {
                    priority: 20,
                    routes: vec!["route-a".into()],
                },
            ],
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("more than one tier"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn cannot_delete_default_group() {
        let path = temp_config();
        let error = remove_model_group(&path, "llmgateway-auto").unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot delete default model group"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
