use crate::config::AppConfig;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    SqlitePool,
};
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

    if result.rows_affected() == 0 {
        pool.close().await;
        return Err(format!(
            "model '{model_id}' is not registered for account '{account_id}'"
        ));
    }

    sqlx::query(
        "UPDATE models
         SET enabled = CASE
             WHEN EXISTS (
                 SELECT 1 FROM account_models
                 WHERE canonical_model_id = ? AND enabled = 1
             ) THEN 1 ELSE 0 END,
             updated_at = CURRENT_TIMESTAMP
         WHERE canonical_id = ?",
    )
    .bind(model_id)
    .bind(model_id)
    .execute(&pool)
    .await
    .map_err(|error| error.to_string())?;

    pool.close().await;
    Ok(())
}

pub async fn set_model_enabled(
    config: &AppConfig,
    model_id: &str,
    enabled: bool,
) -> Result<u64, String> {
    let pool = catalog_pool(config).await?;
    let exists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM models WHERE canonical_id = ?")
        .bind(model_id)
        .fetch_one(&pool)
        .await
        .map_err(|error| error.to_string())?;
    if exists == 0 {
        pool.close().await;
        return Err(format!("unknown model '{model_id}'"));
    }

    let result = sqlx::query(
        "UPDATE models SET enabled = ?, updated_at = CURRENT_TIMESTAMP WHERE canonical_id = ?",
    )
    .bind(if enabled { 1_i64 } else { 0_i64 })
    .bind(model_id)
    .execute(&pool)
    .await
    .map_err(|error| error.to_string())?;

    if enabled {
        sqlx::query("UPDATE account_models SET enabled = 1 WHERE canonical_model_id = ?")
            .bind(model_id)
            .execute(&pool)
            .await
            .map_err(|error| error.to_string())?;
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ModelCatalog;
    use crate::live_config::LiveConfig;
    use std::sync::Arc;

    #[tokio::test]
    async fn set_account_model_and_model_enabled_sync_both_directions() {
        let temp_db = std::env::temp_dir().join(format!("llm-test-{}.db", Uuid::new_v4().simple()));
        let db_url = format!("sqlite://{}", temp_db.display()).replace('\\', "\\\\");

        let config_toml = format!(
            r#"
[server]
host = "127.0.0.1"
port = 7331

[api]
default_model = "test-model"

[[providers]]
id = "test-provider"
kind = "openai-compatible"
base_url = "https://api.test.com"

[[accounts]]
id = "test-account"
provider = "test-provider"
api_key_env = "TEST_API_KEY"
enabled = true

[[routes]]
id = "test-route"
model = "model-1"
account = "test-account"

[virtual_models.test-model]
routes = ["test-route"]

[storage]
database_url = "{db_url}"
"#
        );
        let config = Arc::new(AppConfig::parse(&config_toml).unwrap());
        let live_config = LiveConfig::new(config.clone());
        let _catalog = ModelCatalog::connect(live_config).await.unwrap();
        let pool = catalog_pool(&config).await.unwrap();

        // Seed a model and account_model
        sqlx::query(
            "INSERT INTO models (canonical_id, provider_id, external_id, display_name, owned_by, enabled)
             VALUES ('test-provider/model-1', 'test-provider', 'model-1', 'Model 1', 'test', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO account_models (account_id, canonical_model_id, availability, enabled, configured, discovered)
             VALUES ('test-account', 'test-provider/model-1', 'available', 1, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // 1. Disable via set_account_model_enabled -> models.enabled should become 0
        set_account_model_enabled(&config, "test-account", "test-provider/model-1", false)
            .await
            .unwrap();

        let am_enabled: i64 = sqlx::query_scalar(
            "SELECT enabled FROM account_models WHERE account_id = 'test-account' AND canonical_model_id = 'test-provider/model-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(am_enabled, 0);

        let m_enabled: i64 = sqlx::query_scalar(
            "SELECT enabled FROM models WHERE canonical_id = 'test-provider/model-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            m_enabled, 0,
            "models.enabled must sync to 0 when account model disabled"
        );

        // 2. Enable via set_model_enabled -> account_models.enabled should become 1
        set_model_enabled(&config, "test-provider/model-1", true)
            .await
            .unwrap();

        let am_enabled: i64 = sqlx::query_scalar(
            "SELECT enabled FROM account_models WHERE account_id = 'test-account' AND canonical_model_id = 'test-provider/model-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            am_enabled, 1,
            "account_models.enabled must sync to 1 when model enabled"
        );

        let m_enabled: i64 = sqlx::query_scalar(
            "SELECT enabled FROM models WHERE canonical_id = 'test-provider/model-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(m_enabled, 1);

        pool.close().await;
        let _ = fs::remove_file(&temp_db);
    }
}
