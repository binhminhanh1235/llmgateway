use crate::{
    config::AppConfig,
    structured_memory::{StructuredMemory, MEMORY_SCHEMA_VERSION},
};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Row,
};
use std::{error::Error, str::FromStr};

pub async fn backfill_legacy_memories(config: &AppConfig) -> Result<usize, Box<dyn Error>> {
    let options = SqliteConnectOptions::from_str(&config.storage.database_url)?
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await?;

    let rows = sqlx::query(
        "SELECT c.thread_id, c.through_ordinal, c.summary, c.summary_model, c.route_id
         FROM context_checkpoints c
         INNER JOIN (
             SELECT thread_id, MAX(through_ordinal) AS through_ordinal
             FROM context_checkpoints
             GROUP BY thread_id
         ) latest
           ON latest.thread_id = c.thread_id
          AND latest.through_ordinal = c.through_ordinal
         LEFT JOIN thread_memories m ON m.thread_id = c.thread_id
         WHERE m.thread_id IS NULL",
    )
    .fetch_all(&pool)
    .await?;

    let mut backfilled = 0usize;
    for row in rows {
        let thread_id: String = row.try_get("thread_id")?;
        let through_ordinal: i64 = row.try_get("through_ordinal")?;
        let summary: String = row.try_get("summary")?;
        let model: String = row.try_get("summary_model")?;
        let route_id: Option<String> = row.try_get("route_id")?;
        let memory = StructuredMemory::from_legacy_text(summary);
        let memory_json = serde_json::to_string(&memory)?;

        sqlx::query(
            "INSERT OR IGNORE INTO thread_memories
             (thread_id, through_ordinal, schema_version, memory_json, model, route_id)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(thread_id)
        .bind(through_ordinal)
        .bind(MEMORY_SCHEMA_VERSION)
        .bind(memory_json)
        .bind(model)
        .bind(route_id)
        .execute(&pool)
        .await?;
        backfilled += 1;
    }

    pool.close().await;
    Ok(backfilled)
}
