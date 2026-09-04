use crate::{
    api::{authorize, json_error, json_response, AppState},
    browser_provider::BrowserProviderConfig,
    browser_provider_runtime,
    browser_session::BrowserConfig,
    browser_session_runtime,
    chromium_driver::ChromiumConfig,
    chromium_driver_runtime,
    config::AppConfig,
};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use toml_edit::{
    value, Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value,
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize)]
pub struct BrowserAccountProviderPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub provider_id: &'static str,
    pub provider_kind: &'static str,
    pub login_url: &'static str,
    pub ready_url_prefix: &'static str,
    pub default_model_id: &'static str,
    pub default_capabilities: &'static [&'static str],
}

#[derive(Debug, Deserialize)]
pub struct BrowserAccountEnabledRequest {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateBrowserAccountRequest {
    pub provider: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub model_label: Option<String>,
    #[serde(default)]
    pub priority: Option<i32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserAccountSetupResult {
    pub provider: String,
    pub account_id: String,
    pub session_id: String,
    pub route_id: String,
    pub model_id: String,
    pub config_path: String,
    pub backup_path: Option<String>,
    pub restart_required: bool,
    pub next_steps: Vec<String>,
}

#[derive(Debug, Error)]
pub enum BrowserAccountSetupError {
    #[error("browser account setup storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("browser account setup TOML error: {0}")]
    TomlEdit(#[from] toml_edit::TomlError),
    #[error("browser account setup produced invalid gateway configuration: {0}")]
    InvalidGatewayConfig(#[from] crate::config::ConfigError),
    #[error("invalid browser account setup: {0}")]
    Invalid(String),
    #[error("browser account '{0}' already exists")]
    Conflict(String),
    #[error("browser account hot activation failed: {0}")]
    Activation(String),
}

pub async fn browser_account_setup_presets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    json_response(
        StatusCode::OK,
        json!({
            "providers": provider_presets(),
            "hot_activation": true,
            "restart_required_after_create": false
        }),
        None,
    )
}

pub async fn set_browser_account_enabled(
    State(state): State<AppState>,
    axum::extract::Path(account_id): axum::extract::Path<String>,
    headers: HeaderMap,
    Json(body): Json<BrowserAccountEnabledRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let config_path =
        env::var("LLMGATEWAY_CONFIG").unwrap_or_else(|_| "config/llmgateway.toml".into());
    match apply_browser_account_enabled(&config_path, &account_id, body.enabled) {
        Ok(()) => match activate_browser_account_setup(&state, &config_path).await {
            Ok(()) => json_response(
                StatusCode::OK,
                json!({
                    "account_id": account_id,
                    "enabled": body.enabled,
                    "restart_required": false
                }),
                None,
            ),
            Err(error) => json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "browser_account_hot_activation_error",
                &error.to_string(),
            ),
        },
        Err(BrowserAccountSetupError::Invalid(message)) => json_error(
            StatusCode::BAD_REQUEST,
            "browser_account_setup_error",
            &message,
        ),
        Err(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "browser_account_setup_error",
            &error.to_string(),
        ),
    }
}

pub async fn create_browser_account_setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateBrowserAccountRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let config_path =
        env::var("LLMGATEWAY_CONFIG").unwrap_or_else(|_| "config/llmgateway.toml".into());
    match apply_browser_account_setup(&config_path, body) {
        Ok(mut result) => match activate_browser_account_setup(&state, &config_path).await {
            Ok(()) => {
                result.restart_required = false;
                result.next_steps = vec![
                    "Open Accounts and choose Login with browser for the new account.".into(),
                    "Complete provider login, CAPTCHA, and 2FA normally in Chromium if requested.".into(),
                    "Verify the authenticated page; browser-first routing will make the route eligible immediately.".into(),
                ];
                json_response(StatusCode::CREATED, json!(result), None)
            }
            Err(error) => json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "browser_account_hot_activation_error",
                &error.to_string(),
            ),
        },
        Err(BrowserAccountSetupError::Conflict(message)) => json_error(
            StatusCode::CONFLICT,
            "browser_account_setup_conflict",
            &format!("browser account '{message}' already exists"),
        ),
        Err(BrowserAccountSetupError::Invalid(message)) => json_error(
            StatusCode::BAD_REQUEST,
            "browser_account_setup_error",
            &message,
        ),
        Err(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "browser_account_setup_error",
            &error.to_string(),
        ),
    }
}

async fn activate_browser_account_setup(
    state: &AppState,
    config_path: &str,
) -> Result<(), BrowserAccountSetupError> {
    let app_config = Arc::new(AppConfig::load(config_path)?);
    let browser_config = BrowserConfig::load_from_gateway_config(config_path)
        .map_err(|error| BrowserAccountSetupError::Activation(error.to_string()))?;
    let chromium_config = ChromiumConfig::load_from_gateway_config(config_path)
        .map_err(|error| BrowserAccountSetupError::Activation(error.to_string()))?;
    let provider_config = BrowserProviderConfig::load_from_gateway_config(config_path)
        .map_err(|error| BrowserAccountSetupError::Activation(error.to_string()))?;

    let browser_sessions = browser_session_runtime::get().ok_or_else(|| {
        BrowserAccountSetupError::Activation("browser session runtime is not initialized".into())
    })?;
    let chromium_driver = chromium_driver_runtime::get().ok_or_else(|| {
        BrowserAccountSetupError::Activation("Chromium driver runtime is not initialized".into())
    })?;
    let browser_providers = browser_provider_runtime::get().ok_or_else(|| {
        BrowserAccountSetupError::Activation("browser provider runtime is not initialized".into())
    })?;

    browser_sessions
        .reload(browser_config)
        .await
        .map_err(|error| BrowserAccountSetupError::Activation(error.to_string()))?;
    chromium_driver
        .reload(chromium_config)
        .map_err(|error| BrowserAccountSetupError::Activation(error.to_string()))?;
    browser_providers
        .reload(provider_config)
        .map_err(|error| BrowserAccountSetupError::Activation(error.to_string()))?;

    state
        .catalog
        .seed_from_app_config(app_config.as_ref())
        .await
        .map_err(|error| BrowserAccountSetupError::Activation(error.to_string()))?;

    state.gateway.live_config.replace(app_config);
    Ok(())
}

pub fn apply_browser_account_enabled(
    path: impl AsRef<Path>,
    account_id: &str,
    enabled: bool,
) -> Result<(), BrowserAccountSetupError> {
    validate_id("account_id", account_id)?;
    let path = path.as_ref();
    let raw = fs::read_to_string(path)?;
    let current = AppConfig::parse(&raw)?;
    let account = current.account(account_id).ok_or_else(|| {
        BrowserAccountSetupError::Invalid(format!("unknown account '{account_id}'"))
    })?;
    let provider = current.provider(&account.provider).ok_or_else(|| {
        BrowserAccountSetupError::Invalid(format!(
            "account '{account_id}' references unknown provider '{}'",
            account.provider
        ))
    })?;
    if !provider.is_browser() {
        return Err(BrowserAccountSetupError::Invalid(format!(
            "account '{account_id}' is not a browser account"
        )));
    }

    let mut doc = raw.parse::<DocumentMut>()?;
    let managed = doc
        .get("browser")
        .and_then(Item::as_table)
        .and_then(|browser| browser.get("bindings"))
        .and_then(Item::as_table)
        .is_some_and(|bindings| bindings.contains_key(account_id));
    if !managed {
        return Err(BrowserAccountSetupError::Invalid(format!(
            "browser account '{account_id}' is not managed by the browser account wizard"
        )));
    }

    let accounts = ensure_aot(doc.as_table_mut(), "accounts")?;
    let table = accounts
        .iter_mut()
        .find(|table| table.get("id").and_then(Item::as_str) == Some(account_id))
        .ok_or_else(|| {
            BrowserAccountSetupError::Invalid(format!("unknown account '{account_id}'"))
        })?;
    table["enabled"] = value(enabled);

    let rendered = doc.to_string();
    AppConfig::parse(&rendered)?;
    write_validated_config(path, &rendered)?;
    Ok(())
}

pub fn apply_browser_account_setup(
    path: impl AsRef<Path>,
    request: CreateBrowserAccountRequest,
) -> Result<BrowserAccountSetupResult, BrowserAccountSetupError> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path)?;
    let current = AppConfig::parse(&raw)?;
    let preset = provider_preset(request.provider.trim()).ok_or_else(|| {
        BrowserAccountSetupError::Invalid(format!(
            "unsupported browser provider '{}'; choose gemini or qwen",
            request.provider
        ))
    })?;

    let account_id = request
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "{}-web-{}",
                preset.id,
                Uuid::new_v4().simple().to_string()[..8].to_string()
            )
        });
    validate_id("account_id", &account_id)?;
    if current.accounts.iter().any(|account| account.id == account_id) {
        return Err(BrowserAccountSetupError::Conflict(account_id));
    }

    let session_id = account_id.clone();
    let route_id = format!("{account_id}-route");
    let model_id = request
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(preset.default_model_id)
        .to_string();
    validate_id("model_id", &model_id)?;
    validate_id("route_id", &route_id)?;

    let label = request
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&account_id)
        .to_string();
    let model_label = request
        .model_label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let priority = request.priority.unwrap_or(10);
    if !(-10_000..=10_000).contains(&priority) {
        return Err(BrowserAccountSetupError::Invalid(
            "priority must be between -10000 and 10000".into(),
        ));
    }

    let mut doc = raw.parse::<DocumentMut>()?;

    {
        let browser = ensure_table(doc.as_table_mut(), "browser")?;
        browser["enabled"] = value(true);
        if !browser.contains_key("profile_root") {
            browser["profile_root"] = value("data/browser-profiles");
        }
        let sessions = ensure_table(browser, "sessions")?;
        if sessions.contains_key(&session_id) {
            return Err(BrowserAccountSetupError::Conflict(account_id));
        }
        let session = ensure_table(sessions, &session_id)?;
        session["provider"] = value(preset.provider_id);
        session["label"] = value(&label);
        session["login_url"] = value(preset.login_url);
        session["enabled"] = value(true);

        let bindings = ensure_table(browser, "bindings")?;
        let binding = ensure_table(bindings, &account_id)?;
        binding["session"] = value(&session_id);
        binding["adapter_contract_version"] = value(1);
        binding["models"] = Item::Value(Value::Array(string_array([model_id.as_str()])));
        binding["ephemeral_chat"] = value(true);
        binding["probe_timeout_ms"] = value(3_000);
        binding["response_timeout_ms"] = value(180_000);
        if let Some(model_label) = &model_label {
            let mut labels = InlineTable::new();
            labels.insert(&model_id, Value::from(model_label.as_str()));
            binding["model_labels"] = Item::Value(Value::InlineTable(labels));
        }
    }

    {
        let chromium = ensure_table(doc.as_table_mut(), "chromium")?;
        chromium["enabled"] = value(true);
        if !chromium.contains_key("startup_timeout_seconds") {
            chromium["startup_timeout_seconds"] = value(15);
        }
        if !chromium.contains_key("auto_recover") {
            chromium["auto_recover"] = value(true);
        }
        if !chromium.contains_key("reconcile_interval_seconds") {
            chromium["reconcile_interval_seconds"] = value(15);
        }
        let sessions = ensure_table(chromium, "sessions")?;
        let session = ensure_table(sessions, &session_id)?;
        session["enabled"] = value(true);
        session["ready_url_prefixes"] =
            Item::Value(Value::Array(string_array([preset.ready_url_prefix])));
    }

    {
        let providers = ensure_aot(doc.as_table_mut(), "providers")?;
        let provider = upsert_by_id(providers, preset.provider_id)?;
        match provider
            .get("kind")
            .and_then(Item::as_str)
            .filter(|value| !value.is_empty())
        {
            Some(existing) if existing != preset.provider_kind => {
                return Err(BrowserAccountSetupError::Invalid(format!(
                    "provider '{}' already exists with incompatible kind '{}'",
                    preset.provider_id, existing
                )));
            }
            _ => {
                provider["id"] = value(preset.provider_id);
                provider["kind"] = value(preset.provider_kind);
            }
        }

        let accounts = ensure_aot(doc.as_table_mut(), "accounts")?;
        let account = append_new_by_id(accounts, &account_id)?;
        account["provider"] = value(preset.provider_id);
        account["enabled"] = value(true);
        account["discover_models"] = value(false);

        let routes = ensure_aot(doc.as_table_mut(), "routes")?;
        let route = append_new_by_id(routes, &route_id)?;
        route["account"] = value(&account_id);
        route["model"] = value(&model_id);
        route["priority"] = value(i64::from(priority));
        route["enabled"] = value(true);
        route["capabilities"] = Item::Value(Value::Array(string_array(
            preset.default_capabilities.iter().copied(),
        )));
    }

    {
        let virtual_models = ensure_table(doc.as_table_mut(), "virtual_models")?;
        let default_model = ensure_table(virtual_models, &current.api.default_model)?;
        append_unique_string(default_model, "routes", &route_id)?;

        if preset.id == "qwen" && virtual_models.contains_key("llmgateway-coding") {
            let coding = ensure_table(virtual_models, "llmgateway-coding")?;
            append_unique_string(coding, "routes", &route_id)?;
        }
        if preset.id == "gemini" && virtual_models.contains_key("llmgateway-best") {
            let best = ensure_table(virtual_models, "llmgateway-best")?;
            append_unique_string(best, "routes", &route_id)?;
        }
    }

    let rendered = doc.to_string();
    AppConfig::parse(&rendered)?;
    let backup_path = write_validated_config(path, &rendered)?;

    Ok(BrowserAccountSetupResult {
        provider: preset.id.to_string(),
        account_id,
        session_id,
        route_id,
        model_id,
        config_path: path.display().to_string(),
        backup_path: backup_path.map(|path| path.display().to_string()),
        restart_required: true,
        next_steps: vec![
            "Restart llmgateway so the managed browser account becomes active.".into(),
            "Open Accounts and choose Login with browser for the new account.".into(),
            "Complete provider login, CAPTCHA, and 2FA normally in Chromium if requested.".into(),
            "Verify the authenticated page; browser-first routing will then make the route eligible."
                .into(),
        ],
    })
}

pub fn provider_presets() -> Vec<BrowserAccountProviderPreset> {
    vec![
        provider_preset("gemini").expect("gemini preset"),
        provider_preset("qwen").expect("qwen preset"),
    ]
}

fn provider_preset(id: &str) -> Option<BrowserAccountProviderPreset> {
    match id.trim().to_ascii_lowercase().as_str() {
        "gemini" | "browser-gemini" => Some(BrowserAccountProviderPreset {
            id: "gemini",
            label: "Gemini Web",
            provider_id: "gemini-web",
            provider_kind: "browser-gemini",
            login_url: "https://gemini.google.com/app",
            ready_url_prefix: "https://gemini.google.com/app",
            default_model_id: "gemini-web-default",
            default_capabilities: &["chat", "reasoning", "long-context"],
        }),
        "qwen" | "browser-qwen" => Some(BrowserAccountProviderPreset {
            id: "qwen",
            label: "Qwen Web",
            provider_id: "qwen-web",
            provider_kind: "browser-qwen",
            login_url: "https://chat.qwen.ai/",
            ready_url_prefix: "https://chat.qwen.ai/",
            default_model_id: "qwen-web-default",
            default_capabilities: &["chat", "coding", "reasoning"],
        }),
        _ => None,
    }
}

fn validate_id(field: &str, value: &str) -> Result<(), BrowserAccountSetupError> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err(BrowserAccountSetupError::Invalid(format!(
            "{field} must be 1-96 characters using only letters, numbers, '.', '_' or '-'"
        )));
    }
    Ok(())
}

fn ensure_table<'a>(
    parent: &'a mut Table,
    key: &str,
) -> Result<&'a mut Table, BrowserAccountSetupError> {
    if !parent.contains_key(key) {
        parent.insert(key, Item::Table(Table::new()));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| {
            BrowserAccountSetupError::Invalid(format!(
                "configuration key '{key}' must be a TOML table"
            ))
        })
}

fn ensure_aot<'a>(
    parent: &'a mut Table,
    key: &str,
) -> Result<&'a mut ArrayOfTables, BrowserAccountSetupError> {
    if !parent.contains_key(key) {
        parent.insert(key, Item::ArrayOfTables(ArrayOfTables::new()));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| {
            BrowserAccountSetupError::Invalid(format!(
                "configuration key '{key}' must be an array of TOML tables"
            ))
        })
}

fn upsert_by_id<'a>(
    tables: &'a mut ArrayOfTables,
    id: &str,
) -> Result<&'a mut Table, BrowserAccountSetupError> {
    let existing_index = {
        tables
            .iter()
            .position(|table| table.get("id").and_then(Item::as_str) == Some(id))
    };
    if let Some(index) = existing_index {
        return tables.get_mut(index).ok_or_else(|| {
            BrowserAccountSetupError::Invalid(format!("could not update table '{id}'"))
        });
    }
    let mut table = Table::new();
    table["id"] = value(id);
    tables.push(table);
    let index = tables.len().saturating_sub(1);
    tables.get_mut(index).ok_or_else(|| {
        BrowserAccountSetupError::Invalid(format!("could not create table '{id}'"))
    })
}

fn append_new_by_id<'a>(
    tables: &'a mut ArrayOfTables,
    id: &str,
) -> Result<&'a mut Table, BrowserAccountSetupError> {
    if tables
        .iter()
        .any(|table| table.get("id").and_then(Item::as_str) == Some(id))
    {
        return Err(BrowserAccountSetupError::Conflict(id.to_string()));
    }
    let mut table = Table::new();
    table["id"] = value(id);
    tables.push(table);
    let index = tables.len().saturating_sub(1);
    tables.get_mut(index).ok_or_else(|| {
        BrowserAccountSetupError::Invalid(format!("could not create table '{id}'"))
    })
}

fn append_unique_string(
    table: &mut Table,
    key: &str,
    value_to_add: &str,
) -> Result<(), BrowserAccountSetupError> {
    if !table.contains_key(key) {
        table[key] = Item::Value(Value::Array(Array::new()));
    }
    let array = table
        .get_mut(key)
        .and_then(Item::as_array_mut)
        .ok_or_else(|| {
            BrowserAccountSetupError::Invalid(format!(
                "configuration key '{key}' must be an array"
            ))
        })?;
    if !array.iter().any(|value| value.as_str() == Some(value_to_add)) {
        array.push(value_to_add);
    }
    Ok(())
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
) -> Result<Option<PathBuf>, BrowserAccountSetupError> {
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
        ".llmgateway-config-{}.tmp",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> String {
        r#"
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[context]
enabled = false
retrieval_enabled = false

[routing]
execution_preference = "browser-first"
api_fallback = true

[[providers]]
id = "api"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"

[[accounts]]
id = "api"
provider = "api"
api_key_env = "FAKE_API_KEY"
enabled = true
discover_models = false

[[routes]]
id = "api"
account = "api"
model = "fake-model"
priority = 100
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["api"]

[virtual_models.llmgateway-coding]
routes = ["api"]

[virtual_models.llmgateway-best]
routes = ["api"]
"#
        .trim_start()
        .to_string()
    }

    fn temp_config() -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "llmgateway-browser-setup-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("llmgateway.toml");
        fs::write(&path, minimal_config()).unwrap();
        path
    }

    #[test]
    fn creates_complete_gemini_browser_account_config() {
        let path = temp_config();
        let result = apply_browser_account_setup(
            &path,
            CreateBrowserAccountRequest {
                provider: "gemini".into(),
                account_id: Some("gemini-a".into()),
                label: Some("Gemini A".into()),
                model_id: Some("gemini-web-pro".into()),
                model_label: Some("Pro".into()),
                priority: Some(5),
            },
        )
        .unwrap();

        assert_eq!(result.account_id, "gemini-a");
        assert!(result.restart_required);
        let raw = fs::read_to_string(&path).unwrap();
        let parsed = AppConfig::parse(&raw).unwrap();
        let provider = parsed.provider("gemini-web").unwrap();
        assert_eq!(provider.kind, "browser-gemini");
        let account = parsed.account("gemini-a").unwrap();
        assert_eq!(account.provider, "gemini-web");
        let route = parsed.route("gemini-a-route").unwrap();
        assert_eq!(route.model, "gemini-web-pro");
        assert_eq!(route.priority, 5);
        assert!(parsed.virtual_models["llmgateway-auto"]
            .routes
            .contains(&"gemini-a-route".to_string()));
        assert!(parsed.virtual_models["llmgateway-best"]
            .routes
            .contains(&"gemini-a-route".to_string()));
        assert!(raw.contains("[browser.sessions.gemini-a]"));
        assert!(raw.contains("[browser.bindings.gemini-a]"));
        assert!(raw.contains("[chromium.sessions.gemini-a]"));
        assert!(raw.contains("model_labels"));

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn creates_qwen_route_for_coding_virtual_model() {
        let path = temp_config();
        apply_browser_account_setup(
            &path,
            CreateBrowserAccountRequest {
                provider: "qwen".into(),
                account_id: Some("qwen-a".into()),
                label: None,
                model_id: None,
                model_label: None,
                priority: None,
            },
        )
        .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let parsed = AppConfig::parse(&raw).unwrap();
        assert_eq!(parsed.provider("qwen-web").unwrap().kind, "browser-qwen");
        assert!(parsed.virtual_models["llmgateway-coding"]
            .routes
            .contains(&"qwen-a-route".to_string()));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rejects_duplicate_account_id_without_overwriting_config() {
        let path = temp_config();
        let request = || CreateBrowserAccountRequest {
            provider: "gemini".into(),
            account_id: Some("gemini-a".into()),
            label: None,
            model_id: None,
            model_label: None,
            priority: None,
        };
        apply_browser_account_setup(&path, request()).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        assert!(matches!(
            apply_browser_account_setup(&path, request()),
            Err(BrowserAccountSetupError::Conflict(_))
        ));
        assert_eq!(first, fs::read_to_string(&path).unwrap());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
