use crate::config::AppConfig;
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, SqlitePool};
use std::str::FromStr;

pub async fn set_account_model_enabled(
    config: &AppConfig,
    account_id: &str,
    model_id: &str,
    enabled: bool,
) -> Result<(), String> {
    if config.account(account_id).is_none() {
        return Err(format!("unknown account '{account_id}'"));
    }

    let options = SqliteConnectOptions::from_str(&config.storage.database_url)
        .map_err(|error| error.to_string())?
        .create_if_missing(false)
        .foreign_keys(true);
    let pool: SqlitePool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|error| error.to_string())?;

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
