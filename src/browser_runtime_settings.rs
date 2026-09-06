use crate::{
    api::{authorize, json_error, json_response, AppState},
    chromium_driver::{
        discover_compatible_browsers, resolve_executable, same_executable, ChromiumConfig,
    },
    chromium_driver_runtime,
};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::{env, fs, path::Path};
use toml_edit::{value, DocumentMut, Item, Table};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct BrowserRuntimeSelectionRequest {
    pub browser_id: String,
}

pub async fn browser_runtime_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }
    match runtime_view() {
        Ok(value) => json_response(StatusCode::OK, value, None),
        Err(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "browser_runtime_settings_error",
            &error,
        ),
    }
}

pub async fn set_browser_runtime_selection(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<BrowserRuntimeSelectionRequest>,
) -> Response<Body> {
    if let Err(response) = authorize(&headers, &state.gateway_api_key) {
        return response;
    }

    let browser_id = request.browser_id.trim();
    if browser_id.is_empty() {
        return json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "browser_runtime_selection_error",
            "browser_id must be a detected browser id or 'auto'",
        );
    }

    let browsers = discover_compatible_browsers();
    let selected = if browser_id == "auto" {
        None
    } else {
        match browsers.iter().find(|browser| browser.id == browser_id) {
            Some(browser) => Some(browser),
            None => {
                return json_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "browser_runtime_selection_error",
                    "selected browser is not currently installed or compatible",
                )
            }
        }
    };

    let config_path =
        env::var("LLMGATEWAY_CONFIG").unwrap_or_else(|_| "config/llmgateway.toml".into());
    if let Err(error) = persist_selection(
        Path::new(&config_path),
        selected.map(|browser| browser.executable.as_str()),
    ) {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "browser_runtime_selection_error",
            &error,
        );
    }

    let config = match ChromiumConfig::load_from_gateway_config(&config_path) {
        Ok(config) => config,
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "browser_runtime_config_error",
                &error.to_string(),
            )
        }
    };
    if let Some(driver) = chromium_driver_runtime::get() {
        if let Err(error) = driver.reload(config) {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "browser_runtime_reload_error",
                &error.to_string(),
            );
        }
    }

    match runtime_view() {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert("updated".into(), json!(true));
                object.insert("restart_required".into(), json!(false));
                object.insert("applies_to".into(), json!("next_browser_launch"));
            }
            json_response(StatusCode::OK, value, None)
        }
        Err(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "browser_runtime_settings_error",
            &error,
        ),
    }
}

fn runtime_view() -> Result<serde_json::Value, String> {
    let config_path =
        env::var("LLMGATEWAY_CONFIG").unwrap_or_else(|_| "config/llmgateway.toml".into());
    let config = ChromiumConfig::load_from_gateway_config(&config_path)
        .map_err(|error| error.to_string())?;
    let browsers = discover_compatible_browsers();
    let effective = resolve_executable(config.executable.as_deref()).ok();
    let selected_id = effective.as_deref().and_then(|executable| {
        browsers
            .iter()
            .find(|browser| same_executable(&browser.executable, executable))
            .map(|browser| browser.id.clone())
    });

    let browser_views = browsers
        .iter()
        .map(|browser| {
            json!({
                "id": browser.id,
                "label": browser.label,
                "product": browser.product,
                "executable": browser.executable,
                "recommended": browser.recommended,
                "selected": selected_id.as_deref() == Some(browser.id.as_str())
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "object": "llmgateway.browser.runtime.settings",
        "driver_enabled": config.enabled,
        "selection_mode": if config.executable.is_some() { "explicit" } else { "auto" },
        "selected_browser_id": selected_id,
        "effective_executable": effective,
        "detected_count": browser_views.len(),
        "browsers": browser_views,
        "auto_preference": ["google-chrome", "microsoft-edge", "brave", "chromium"]
    }))
}

fn persist_selection(path: &Path, executable: Option<&str>) -> Result<(), String> {
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut doc = raw
        .parse::<DocumentMut>()
        .map_err(|error| error.to_string())?;

    if !doc.as_table().contains_key("chromium") {
        doc.as_table_mut()
            .insert("chromium", Item::Table(Table::new()));
    }
    let runtime = doc
        .get_mut("chromium")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| "configuration key 'chromium' must be a TOML table".to_string())?;

    match executable {
        Some(executable) => runtime["executable"] = value(executable),
        None => {
            runtime.remove("executable");
        }
    }

    let rendered = doc.to_string();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temp = parent.join(format!(
        ".llmgateway-browser-runtime-{}.tmp",
        Uuid::new_v4().simple()
    ));
    fs::write(&temp, rendered).map_err(|error| error.to_string())?;
    if let Err(error) = fs::rename(&temp, path) {
        if error.kind() == std::io::ErrorKind::AlreadyExists
            || error.kind() == std::io::ErrorKind::PermissionDenied
        {
            fs::copy(&temp, path).map_err(|copy_error| copy_error.to_string())?;
            let _ = fs::remove_file(&temp);
        } else {
            let _ = fs::remove_file(&temp);
            return Err(error.to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::persist_selection;
    use std::fs;

    #[test]
    fn browser_selection_can_be_set_and_reset_to_auto() {
        let root = std::env::temp_dir().join(format!(
            "llmgateway-browser-runtime-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("gateway.toml");
        fs::write(
            &path,
            "[server]\nhost='127.0.0.1'\nport=7331\n[chromium]\nenabled=true\n",
        )
        .unwrap();

        persist_selection(&path, Some("/tmp/google-chrome")).unwrap();
        let selected = fs::read_to_string(&path).unwrap();
        assert!(selected.contains("executable = \"/tmp/google-chrome\""));

        persist_selection(&path, None).unwrap();
        let auto = fs::read_to_string(&path).unwrap();
        assert!(!auto.contains("executable ="));

        let _ = fs::remove_dir_all(root);
    }
}
