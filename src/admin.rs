use crate::config::AppConfig;
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, SqlitePool};
use std::{fs, path::Path, str::FromStr};
use toml_edit::{value, DocumentMut, Item};
use uuid::Uuid;

async fn catalog_pool(config: &AppConfig) -> Result<SqlitePool, String> {
    let options = SqliteConnectOptions::from_str(&config.storage.database_url)
        .map_err(|error| error.to_string())?
        .create_if_missing(false)
        .foreign_keys(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|error| error.to_string())
}

pub async fn set_account_model_enabled(
    config: &AppConfig,
    account_id: &str,
    model_id: &str,
    enabled: bool,
) -> Result<(), String> {
    if config.account(account_id).is_none() {
        return Err(format!("unknown account '{account_id}'"));
    }

    let pool = catalog_pool(config).await?;
    let result = sqlx::query(
        "UPDATE account_models SET enabled = ? WHERE account_id = ? AND canonical_model_id = ?",
    )
    .bind(if enabled { 1_i64 } else { 0_i64 })
    .bind(account_id)
    .bind(model_id)
    .execute(&pool)
    .await
    .map_err(|error| error.to_string())?;
    pool.close().await;

    if result.rows_affected() == 0 {
        return Err(format!(
            "model '{model_id}' is not registered for account '{account_id}'"
        ));
    }
    Ok(())
}

pub async fn set_model_enabled(
    config: &AppConfig,
    model_id: &str,
    enabled: bool,
) -> Result<u64, String> {
    let pool = catalog_pool(config).await?;
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM models WHERE canonical_id = ?",
    )
    .bind(model_id)
    .fetch_one(&pool)
    .await
    .map_err(|error| error.to_string())?;
    if exists == 0 {
        pool.close().await;
        return Err(format!("unknown model '{model_id}'"));
    }

    let result = sqlx::query(
        "UPDATE account_models SET enabled = ? WHERE canonical_model_id = ?",
    )
    .bind(if enabled { 1_i64 } else { 0_i64 })
    .bind(model_id)
    .execute(&pool)
    .await
    .map_err(|error| error.to_string())?;
    pool.close().await;
    Ok(result.rows_affected())
}

pub fn set_account_enabled_in_config(
    path: impl AsRef<Path>,
    account_id: &str,
    enabled: bool,
) -> Result<AppConfig, String> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Err("account id cannot be empty".into());
    }

    let path = path.as_ref();
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let current = AppConfig::parse(&raw).map_err(|error| error.to_string())?;
    if current.account(account_id).is_none() {
        return Err(format!("unknown account '{account_id}'"));
    }

    let mut doc = raw
        .parse::<DocumentMut>()
        .map_err(|error| error.to_string())?;
    let accounts = doc
        .get_mut("accounts")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| "configuration has no [[accounts]] array".to_string())?;
    let account = accounts
        .iter_mut()
        .find(|table| table.get("id").and_then(Item::as_str) == Some(account_id))
        .ok_or_else(|| format!("unknown account '{account_id}'"))?;
    account["enabled"] = value(enabled);

    let rendered = doc.to_string();
    let next = AppConfig::parse(&rendered).map_err(|error| error.to_string())?;
    write_config_atomically(path, &rendered)?;
    Ok(next)
}

fn write_config_atomically(path: &Path, rendered: &str) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    if path.exists() {
        let backup = path.with_extension(format!(
            "{}.bak",
            path.extension()
                .and_then(|value| value.to_str())
                .unwrap_or("toml")
        ));
        fs::copy(path, backup).map_err(|error| error.to_string())?;
    }

    let temp = parent.join(format!(
        ".llmgateway-account-state-{}.tmp",
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
