use crate::config::{AppConfig, ArtifactConfig};
use chrono::Utc;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Row, SqlitePool,
};
use std::{
    path::{Component, Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone)]
pub struct ArtifactStore {
    pool: SqlitePool,
    config: ArtifactConfig,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ArtifactRecord {
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub purpose: String,
    pub source: String,
    pub lifecycle_state: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ProviderArtifactBinding {
    pub artifact_id: String,
    pub provider: String,
    pub account_id: String,
    pub provider_file_id: String,
    pub metadata_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("artifact database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("artifact storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid artifact request: {0}")]
    Invalid(String),
    #[error("artifact exceeds configured size limit of {limit} bytes")]
    TooLarge { limit: usize },
    #[error("MIME type '{0}' is denied by artifact policy")]
    MimeDenied(String),
    #[error("declared MIME type '{declared}' does not match detected MIME type '{detected}'")]
    MimeMismatch { declared: String, detected: String },
    #[error("artifact '{0}' was not found")]
    NotFound(String),
    #[error("artifact '{id}' is still referenced by {references} persisted object(s)")]
    InUse { id: String, references: i64 },
}

impl ArtifactStore {
    pub async fn connect(config: Arc<AppConfig>) -> Result<Self, ArtifactError> {
        Self::connect_parts(&config.storage.database_url, config.artifacts.clone()).await
    }

    async fn connect_parts(database_url: &str, config: ArtifactConfig) -> Result<Self, ArtifactError> {
        ensure_sqlite_parent(database_url)?;
        tokio::fs::create_dir_all(&config.root).await?;
        tokio::fs::create_dir_all(Path::new(&config.root).join("blobs")).await?;
        let options = SqliteConnectOptions::from_str(database_url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        let store = Self { pool, config };
        store.migrate().await?;
        store.cleanup_orphan_blobs().await?;
        Ok(store)
    }

    pub fn config(&self) -> &ArtifactConfig {
        &self.config
    }

    async fn migrate(&self) -> Result<(), ArtifactError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS artifact_blobs (
                sha256 TEXT PRIMARY KEY,
                relative_path TEXT NOT NULL UNIQUE,
                size_bytes INTEGER NOT NULL,
                created_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS artifacts (
                id TEXT PRIMARY KEY,
                owner_client_id TEXT,
                filename TEXT NOT NULL,
                mime_type TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                purpose TEXT NOT NULL,
                source TEXT NOT NULL,
                lifecycle_state TEXT NOT NULL,
                created_at TEXT NOT NULL,
                deleted_at TEXT
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_artifacts_owner_state
             ON artifacts(owner_client_id, lifecycle_state, created_at)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_artifacts_sha_state
             ON artifacts(sha256, lifecycle_state)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS artifact_references (
                artifact_id TEXT NOT NULL,
                reference_kind TEXT NOT NULL,
                reference_id TEXT NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY(artifact_id, reference_kind, reference_id),
                FOREIGN KEY(artifact_id) REFERENCES artifacts(id) ON DELETE CASCADE
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS provider_artifact_bindings (
                artifact_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                account_id TEXT NOT NULL,
                provider_file_id TEXT NOT NULL,
                metadata_json TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY(artifact_id, provider, account_id),
                FOREIGN KEY(artifact_id) REFERENCES artifacts(id) ON DELETE CASCADE
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn store_bytes(
        &self,
        owner_client_id: Option<&str>,
        filename: &str,
        declared_mime: Option<&str>,
        purpose: &str,
        source: &str,
        bytes: &[u8],
    ) -> Result<ArtifactRecord, ArtifactError> {
        if bytes.len() > self.config.max_file_size_bytes {
            return Err(ArtifactError::TooLarge {
                limit: self.config.max_file_size_bytes,
            });
        }
        if bytes.is_empty() {
            return Err(ArtifactError::Invalid("file must not be empty".into()));
        }

        let filename = sanitize_filename(filename)?;
        let mime_type = validate_mime(bytes, declared_mime, &self.config)?;
        let sha256 = sha256_hex(bytes);
        let relative_path = blob_relative_path(&sha256);
        let absolute_path = self.safe_blob_path(&relative_path)?;
        if let Some(parent) = absolute_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        if tokio::fs::metadata(&absolute_path).await.is_err() {
            let temp = Path::new(&self.config.root)
                .join(format!(".upload-{}", Uuid::new_v4()));
            tokio::fs::write(&temp, bytes).await?;
            match tokio::fs::rename(&temp, &absolute_path).await {
                Ok(()) => {}
                Err(error) if tokio::fs::metadata(&absolute_path).await.is_ok() => {
                    let _ = tokio::fs::remove_file(&temp).await;
                    drop(error);
                }
                Err(error) => {
                    let _ = tokio::fs::remove_file(&temp).await;
                    return Err(error.into());
                }
            }
        }

        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT OR IGNORE INTO artifact_blobs
             (sha256, relative_path, size_bytes, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&sha256)
        .bind(&relative_path)
        .bind(usize_to_i64(bytes.len()))
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = format!("file_{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO artifacts
             (id, owner_client_id, filename, mime_type, size_bytes, sha256, purpose, source,
              lifecycle_state, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'active', ?)",
        )
        .bind(&id)
        .bind(owner_client_id)
        .bind(&filename)
        .bind(&mime_type)
        .bind(usize_to_i64(bytes.len()))
        .bind(&sha256)
        .bind(purpose.trim())
        .bind(source.trim())
        .bind(&now)
        .execute(&self.pool)
        .await?;

        self.get(&id, owner_client_id, owner_client_id.is_none()).await
    }

    pub async fn get(
        &self,
        id: &str,
        owner_client_id: Option<&str>,
        admin: bool,
    ) -> Result<ArtifactRecord, ArtifactError> {
        let row = if admin {
            sqlx::query(
                "SELECT id, filename, mime_type, size_bytes, sha256, purpose, source,
                        lifecycle_state, created_at
                 FROM artifacts
                 WHERE id = ? AND lifecycle_state = 'active'",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT id, filename, mime_type, size_bytes, sha256, purpose, source,
                        lifecycle_state, created_at
                 FROM artifacts
                 WHERE id = ? AND lifecycle_state = 'active' AND owner_client_id = ?",
            )
            .bind(id)
            .bind(owner_client_id)
            .fetch_optional(&self.pool)
            .await?
        };
        row.map(record_from_row)
            .transpose()?
            .ok_or_else(|| ArtifactError::NotFound(id.to_string()))
    }

    pub async fn read_content(
        &self,
        id: &str,
        owner_client_id: Option<&str>,
        admin: bool,
    ) -> Result<(ArtifactRecord, Vec<u8>), ArtifactError> {
        let record = self.get(id, owner_client_id, admin).await?;
        let row = sqlx::query("SELECT relative_path FROM artifact_blobs WHERE sha256 = ?")
            .bind(&record.sha256)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| ArtifactError::NotFound(id.to_string()))?;
        let relative: String = row.try_get("relative_path")?;
        let path = self.safe_blob_path(&relative)?;
        let bytes = tokio::fs::read(path).await?;
        Ok((record, bytes))
    }

    pub async fn delete(
        &self,
        id: &str,
        owner_client_id: Option<&str>,
        admin: bool,
    ) -> Result<(), ArtifactError> {
        let record = self.get(id, owner_client_id, admin).await?;
        let mut tx = self.pool.begin().await?;
        let references: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM artifact_references WHERE artifact_id = ?",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if references > 0 {
            return Err(ArtifactError::InUse {
                id: id.to_string(),
                references,
            });
        }

        sqlx::query("DELETE FROM provider_artifact_bindings WHERE artifact_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE artifacts
             SET lifecycle_state = 'deleted', deleted_at = ?
             WHERE id = ? AND lifecycle_state = 'active'",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(&mut *tx)
        .await?;

        let active_with_blob: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM artifacts WHERE sha256 = ? AND lifecycle_state = 'active'",
        )
        .bind(&record.sha256)
        .fetch_one(&mut *tx)
        .await?;
        if active_with_blob == 0 {
            let relative: Option<String> = sqlx::query_scalar(
                "SELECT relative_path FROM artifact_blobs WHERE sha256 = ?",
            )
            .bind(&record.sha256)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(relative) = relative {
                let path = self.safe_blob_path(&relative)?;
                match tokio::fs::remove_file(path).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                sqlx::query("DELETE FROM artifact_blobs WHERE sha256 = ?")
                    .bind(&record.sha256)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn sync_references(
        &self,
        reference_kind: &str,
        reference_id: &str,
        artifact_ids: &[String],
    ) -> Result<(), ArtifactError> {
        if reference_kind.trim().is_empty() || reference_id.trim().is_empty() {
            return Err(ArtifactError::Invalid(
                "artifact reference kind and id must not be empty".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM artifact_references
             WHERE reference_kind = ? AND reference_id = ?",
        )
        .bind(reference_kind)
        .bind(reference_id)
        .execute(&mut *tx)
        .await?;
        let now = Utc::now().to_rfc3339();
        for artifact_id in artifact_ids {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM artifacts WHERE id = ? AND lifecycle_state = 'active'",
            )
            .bind(artifact_id)
            .fetch_one(&mut *tx)
            .await?;
            if exists == 0 {
                return Err(ArtifactError::NotFound(artifact_id.clone()));
            }
            sqlx::query(
                "INSERT INTO artifact_references
                 (artifact_id, reference_kind, reference_id, created_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(artifact_id)
            .bind(reference_kind)
            .bind(reference_id)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn upsert_provider_binding(
        &self,
        artifact_id: &str,
        provider: &str,
        account_id: &str,
        provider_file_id: &str,
        metadata_json: Option<&str>,
    ) -> Result<(), ArtifactError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO provider_artifact_bindings
             (artifact_id, provider, account_id, provider_file_id, metadata_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(artifact_id, provider, account_id) DO UPDATE SET
               provider_file_id = excluded.provider_file_id,
               metadata_json = excluded.metadata_json,
               updated_at = excluded.updated_at",
        )
        .bind(artifact_id)
        .bind(provider)
        .bind(account_id)
        .bind(provider_file_id)
        .bind(metadata_json)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn provider_binding(
        &self,
        artifact_id: &str,
        provider: &str,
        account_id: &str,
    ) -> Result<Option<ProviderArtifactBinding>, ArtifactError> {
        let row = sqlx::query(
            "SELECT artifact_id, provider, account_id, provider_file_id, metadata_json,
                    created_at, updated_at
             FROM provider_artifact_bindings
             WHERE artifact_id = ? AND provider = ? AND account_id = ?",
        )
        .bind(artifact_id)
        .bind(provider)
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(ProviderArtifactBinding {
                artifact_id: row.try_get("artifact_id")?,
                provider: row.try_get("provider")?,
                account_id: row.try_get("account_id")?,
                provider_file_id: row.try_get("provider_file_id")?,
                metadata_json: row.try_get("metadata_json")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .transpose()
    }

    pub async fn cleanup_orphan_blobs(&self) -> Result<usize, ArtifactError> {
        let rows = sqlx::query(
            "SELECT b.sha256, b.relative_path
             FROM artifact_blobs b
             LEFT JOIN artifacts a
               ON a.sha256 = b.sha256 AND a.lifecycle_state = 'active'
             WHERE a.id IS NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut removed = 0usize;
        for row in rows {
            let sha256: String = row.try_get("sha256")?;
            let relative: String = row.try_get("relative_path")?;
            let path = self.safe_blob_path(&relative)?;
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            sqlx::query("DELETE FROM artifact_blobs WHERE sha256 = ?")
                .bind(&sha256)
                .execute(&self.pool)
                .await?;
            removed += 1;
        }
        Ok(removed)
    }

    fn safe_blob_path(&self, relative: &str) -> Result<PathBuf, ArtifactError> {
        let relative_path = Path::new(relative);
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(ArtifactError::Invalid(
                "stored artifact path failed safety validation".into(),
            ));
        }
        Ok(Path::new(&self.config.root).join(relative_path))
    }
}

fn record_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ArtifactRecord, sqlx::Error> {
    Ok(ArtifactRecord {
        id: row.try_get("id")?,
        filename: row.try_get("filename")?,
        mime_type: row.try_get("mime_type")?,
        size_bytes: row.try_get("size_bytes")?,
        sha256: row.try_get("sha256")?,
        purpose: row.try_get("purpose")?,
        source: row.try_get("source")?,
        lifecycle_state: row.try_get("lifecycle_state")?,
        created_at: row.try_get("created_at")?,
    })
}

fn sanitize_filename(filename: &str) -> Result<String, ArtifactError> {
    let normalized = filename.replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or("").trim();
    if basename.is_empty() || basename == "." || basename == ".." {
        return Err(ArtifactError::Invalid("filename must contain a safe basename".into()));
    }
    let cleaned = basename
        .chars()
        .filter(|ch| !ch.is_control())
        .take(255)
        .collect::<String>();
    if cleaned.is_empty() {
        return Err(ArtifactError::Invalid("filename contains no safe characters".into()));
    }
    Ok(cleaned)
}

fn validate_mime(
    bytes: &[u8],
    declared: Option<&str>,
    config: &ArtifactConfig,
) -> Result<String, ArtifactError> {
    let declared = declared
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let detected = sniff_mime(bytes, declared.as_deref());

    if let (Some(declared), Some(detected)) = (declared.as_deref(), detected.as_deref()) {
        if declared != "application/octet-stream"
            && declared != detected
            && !equivalent_mime(declared, detected)
        {
            return Err(ArtifactError::MimeMismatch {
                declared: declared.to_string(),
                detected: detected.to_string(),
            });
        }
    }

    let mime = detected
        .or(declared)
        .unwrap_or_else(|| "application/octet-stream".to_string());
    if config
        .denied_mime_types
        .iter()
        .any(|pattern| mime_matches(pattern, &mime))
    {
        return Err(ArtifactError::MimeDenied(mime));
    }
    if !config.allowed_mime_types.is_empty()
        && !config
            .allowed_mime_types
            .iter()
            .any(|pattern| mime_matches(pattern, &mime))
    {
        return Err(ArtifactError::MimeDenied(mime));
    }
    Ok(mime)
}

fn sniff_mime(bytes: &[u8], declared: Option<&str>) -> Option<String> {
    if bytes.starts_with(b"%PDF-") {
        return Some("application/pdf".into());
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png".into());
    }
    if bytes.len() >= 3 && bytes[..3] == [0xff, 0xd8, 0xff] {
        return Some("image/jpeg".into());
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif".into());
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp".into());
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return Some("audio/wav".into());
    }
    if bytes.starts_with(b"ID3")
        || (bytes.len() >= 2 && bytes[0] == 0xff && matches!(bytes[1] & 0xe6, 0xe2 | 0xe4 | 0xe6))
    {
        return Some("audio/mpeg".into());
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return Some("application/zip".into());
    }
    if bytes.starts_with(b"MZ") {
        return Some("application/x-dosexec".into());
    }
    if bytes.starts_with(b"\x7fELF") {
        return Some("application/x-executable".into());
    }
    if bytes.len() >= 4
        && matches!(
            &bytes[..4],
            [0xfe, 0xed, 0xfa, 0xce]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xcf, 0xfa, 0xed, 0xfe]
        )
    {
        return Some("application/x-mach-binary".into());
    }
    if std::str::from_utf8(bytes).is_ok() {
        if let Some(declared) = declared {
            if declared.starts_with("text/")
                || matches!(declared, "application/json" | "application/xml")
            {
                return Some(declared.to_string());
            }
        }
        return Some("text/plain".into());
    }
    None
}

fn equivalent_mime(left: &str, right: &str) -> bool {
    matches!(
        (left, right),
        ("image/jpg", "image/jpeg")
            | ("image/jpeg", "image/jpg")
            | ("audio/x-wav", "audio/wav")
            | ("audio/wav", "audio/x-wav")
    )
}

fn mime_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    if let Some(prefix) = pattern.strip_suffix("/*") {
        value.starts_with(&format!("{prefix}/"))
    } else {
        pattern == value
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn blob_relative_path(sha256: &str) -> String {
    format!("blobs/{}/{}", &sha256[..2], sha256)
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn ensure_sqlite_parent(database_url: &str) -> Result<(), std::io::Error> {
    let Some(path) = database_url.strip_prefix("sqlite://") else {
        return Ok(());
    };
    if path == ":memory:" {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ArtifactConfig;

    fn temp_paths(name: &str) -> (PathBuf, String) {
        let root = std::env::temp_dir().join(format!("llmgateway-{name}-{}", Uuid::new_v4()));
        let db = root.join("state.db");
        let url = format!("sqlite://{}", db.display());
        (root, url)
    }

    fn config(root: &Path) -> ArtifactConfig {
        ArtifactConfig {
            root: root.join("artifacts").display().to_string(),
            max_file_size_bytes: 1024 * 1024,
            max_request_size_bytes: 2 * 1024 * 1024,
            max_files_per_request: 4,
            allowed_mime_types: vec![],
            denied_mime_types: ArtifactConfig::default().denied_mime_types,
            remote_url_ingestion: false,
        }
    }

    #[tokio::test]
    async fn identical_uploads_deduplicate_blob_and_survive_restart() {
        let (root, url) = temp_paths("artifact-dedup");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = config(&root);
        let store = ArtifactStore::connect_parts(&url, cfg.clone()).await.unwrap();
        let a = store
            .store_bytes(Some("client-a"), "a.txt", Some("text/plain"), "assistants", "test", b"same")
            .await
            .unwrap();
        let b = store
            .store_bytes(Some("client-a"), "b.txt", Some("text/plain"), "assistants", "test", b"same")
            .await
            .unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(a.sha256, b.sha256);
        let blob_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifact_blobs")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(blob_count, 1);
        drop(store);

        let reopened = ArtifactStore::connect_parts(&url, cfg).await.unwrap();
        let loaded = reopened.get(&a.id, Some("client-a"), false).await.unwrap();
        assert_eq!(loaded.sha256, a.sha256);
        let (_, content) = reopened
            .read_content(&a.id, Some("client-a"), false)
            .await
            .unwrap();
        assert_eq!(content, b"same");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn owner_isolation_reference_guard_and_binding_cleanup_are_deterministic() {
        let (root, url) = temp_paths("artifact-security");
        std::fs::create_dir_all(&root).unwrap();
        let store = ArtifactStore::connect_parts(&url, config(&root)).await.unwrap();
        let artifact = store
            .store_bytes(Some("client-a"), "safe.txt", Some("text/plain"), "assistants", "test", b"safe")
            .await
            .unwrap();
        assert!(matches!(
            store.get(&artifact.id, Some("client-b"), false).await,
            Err(ArtifactError::NotFound(_))
        ));

        store
            .upsert_provider_binding(&artifact.id, "fake", "account-a", "native-1", None)
            .await
            .unwrap();
        store
            .sync_references("thread_message", "message-1", std::slice::from_ref(&artifact.id))
            .await
            .unwrap();
        assert!(matches!(
            store.delete(&artifact.id, Some("client-a"), false).await,
            Err(ArtifactError::InUse { .. })
        ));

        store
            .sync_references("thread_message", "message-1", &[])
            .await
            .unwrap();
        store.delete(&artifact.id, Some("client-a"), false).await.unwrap();
        assert!(store
            .provider_binding(&artifact.id, "fake", "account-a")
            .await
            .unwrap()
            .is_none());
        assert!(matches!(
            store.get(&artifact.id, Some("client-a"), false).await,
            Err(ArtifactError::NotFound(_))
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejects_oversize_mismatch_and_executable_content_before_metadata_persistence() {
        let (root, url) = temp_paths("artifact-validation");
        std::fs::create_dir_all(&root).unwrap();
        let mut cfg = config(&root);
        cfg.max_file_size_bytes = 4;
        let store = ArtifactStore::connect_parts(&url, cfg).await.unwrap();

        assert!(matches!(
            store
                .store_bytes(None, "big.txt", Some("text/plain"), "assistants", "test", b"12345")
                .await,
            Err(ArtifactError::TooLarge { .. })
        ));
        assert!(matches!(
            store
                .store_bytes(None, "fake.png", Some("image/png"), "assistants", "test", b"%PDF")
                .await,
            Err(ArtifactError::MimeMismatch { .. })
        ));
        assert!(matches!(
            store
                .store_bytes(None, "app.exe", Some("application/octet-stream"), "assistants", "test", b"MZ00")
                .await,
            Err(ArtifactError::MimeDenied(_))
        ));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let _ = std::fs::remove_dir_all(root);
    }
}
