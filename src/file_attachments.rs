use crate::artifact_store::{ArtifactError, ArtifactRecord, ArtifactStore};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const ARTIFACT_FILE_SCHEME: &str = "llmgateway://artifact/";
pub const ATTACHMENT_STRATEGY_FIELD: &str = "llmgateway_attachment_strategy";
const MAX_EXTRACTED_FILE_BYTES: usize = 256 * 1024;
const MAX_EXTRACTED_CHARS: usize = 96 * 1024;

#[derive(Debug, Error)]
pub enum FileAttachmentError {
    #[error("{0}")]
    Artifact(#[from] ArtifactError),
    #[error("invalid file input: {0}")]
    Invalid(String),
    #[error("unsupported capability '{0}'")]
    UnsupportedCapability(String),
    #[error("unsupported file MIME type '{0}'")]
    UnsupportedMime(String),
    #[error("file '{0}' is not valid UTF-8 text")]
    InvalidText(String),
    #[error("file '{0}' exceeds extraction limit")]
    ExtractionTooLarge(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FileSource {
    FileId(String),
    Url(String),
}

pub async fn resolve_file_inputs(
    body: &mut Value,
    store: &ArtifactStore,
    owner_client_id: Option<&str>,
    admin: bool,
) -> Result<Vec<String>, FileAttachmentError> {
    let mut sources = BTreeSet::new();
    collect_file_sources(body, &mut sources);
    let mut replacements = BTreeMap::new();
    let mut artifact_ids = BTreeSet::new();

    for source in sources {
        let (key, record) = match &source {
            FileSource::FileId(id) => {
                let record = store.get(id, owner_client_id, admin).await?;
                ensure_supported_document(&record)?;
                (source_key(&source), record)
            }
            FileSource::Url(url) if url.starts_with(ARTIFACT_FILE_SCHEME) => {
                let id = url.trim_start_matches(ARTIFACT_FILE_SCHEME);
                if id.trim().is_empty() {
                    return Err(FileAttachmentError::Invalid(
                        "artifact file URL is missing a file id".into(),
                    ));
                }
                let record = store.get(id, owner_client_id, admin).await?;
                ensure_supported_document(&record)?;
                (source_key(&source), record)
            }
            FileSource::Url(url) if url.starts_with("file_") => {
                let record = store.get(url, owner_client_id, admin).await?;
                ensure_supported_document(&record)?;
                (source_key(&source), record)
            }
            FileSource::Url(url) if url.starts_with("http://") || url.starts_with("https://") => {
                return Err(FileAttachmentError::UnsupportedCapability(
                    "remote_file_url".into(),
                ));
            }
            FileSource::Url(url) => {
                return Err(FileAttachmentError::Invalid(format!(
                    "file URL must be a file id or {ARTIFACT_FILE_SCHEME} reference; got '{}'",
                    truncate(url, 96)
                )));
            }
        };
        artifact_ids.insert(record.id.clone());
        replacements.insert(
            key,
            (
                format!("{ARTIFACT_FILE_SCHEME}{}", record.id),
                record.filename,
                record.mime_type,
            ),
        );
    }

    rewrite_file_sources(body, &replacements);
    Ok(artifact_ids.into_iter().collect())
}

pub async fn materialize_file_inputs(
    body: &Value,
    store: &ArtifactStore,
    owner_client_id: Option<&str>,
    admin: bool,
) -> Result<Value, FileAttachmentError> {
    let mut ids = BTreeSet::new();
    collect_artifact_file_ids(body, &mut ids);
    let mut replacements = BTreeMap::new();
    let mut saw_extracted = false;
    let mut saw_native = false;

    for id in ids {
        let (record, bytes) = store.read_content(&id, owner_client_id, admin).await?;
        ensure_supported_document(&record)?;
        let replacement = if is_extractable_text_mime(&record.mime_type) {
            saw_extracted = true;
            let extracted = extract_text(&record, &bytes)?;
            json!({
                "type":"text",
                "text":format!(
                    "[Attached file: {} | {} | {} bytes]\n{}",
                    record.filename,
                    record.mime_type,
                    record.size_bytes,
                    extracted
                )
            })
        } else if native_file_mime_supported(&record.mime_type) {
            saw_native = true;
            json!({
                "type":"input_file",
                "file_data":format!(
                    "data:{};base64,{}",
                    record.mime_type,
                    STANDARD.encode(bytes)
                ),
                "filename":record.filename,
                "mime_type":record.mime_type,
                "llmgateway_artifact_id":record.id
            })
        } else {
            return Err(FileAttachmentError::UnsupportedMime(record.mime_type));
        };
        replacements.insert(format!("{ARTIFACT_FILE_SCHEME}{id}"), replacement);
    }

    let mut materialized = body.clone();
    rewrite_materialized_file_inputs(&mut materialized, &replacements);
    if let Some(object) = materialized.as_object_mut() {
        object.remove(ATTACHMENT_STRATEGY_FIELD);
        let strategy = match (saw_extracted, saw_native) {
            (true, true) => Some("mixed"),
            (true, false) => Some("extracted_fallback"),
            (false, true) => Some("native_upload"),
            (false, false) => None,
        };
        if let Some(strategy) = strategy {
            object.insert(
                ATTACHMENT_STRATEGY_FIELD.into(),
                Value::String(strategy.into()),
            );
        }
    }
    Ok(materialized)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderBindingSync {
    pub created: usize,
    pub reused: usize,
}

pub async fn sync_native_provider_bindings(
    body: &Value,
    store: &ArtifactStore,
    provider: &str,
    account_id: &str,
    route_id: &str,
) -> Result<ProviderBindingSync, FileAttachmentError> {
    if !matches!(execution_strategy(body), Some("native_upload" | "mixed")) {
        return Ok(ProviderBindingSync::default());
    }

    let metadata = json!({
        "strategy":"native_upload",
        "binding_kind":"gateway_opaque_native_attachment",
        "native_file_id_available":false,
        "route_id":route_id,
    })
    .to_string();
    let mut summary = ProviderBindingSync::default();
    for artifact_id in file_artifact_ids(body) {
        if store
            .ensure_provider_binding(
                &artifact_id,
                provider,
                account_id,
                Some(&metadata),
            )
            .await?
        {
            summary.reused += 1;
        } else {
            summary.created += 1;
        }
    }
    Ok(summary)
}

pub fn execution_strategy(body: &Value) -> Option<&str> {
    body.get(ATTACHMENT_STRATEGY_FIELD)
        .and_then(Value::as_str)
        .filter(|value| matches!(*value, "native_upload" | "extracted_fallback" | "mixed"))
}

fn rewrite_materialized_file_inputs(
    value: &mut Value,
    replacements: &BTreeMap<String, Value>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                rewrite_materialized_file_inputs(item, replacements);
            }
        }
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "file" | "input_file" | "document") {
                let uri = object
                    .get("file_id")
                    .or_else(|| object.get("artifact_id"))
                    .or_else(|| object.get("file_url"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(replacement) = uri.and_then(|uri| replacements.get(&uri)).cloned() {
                    *value = replacement;
                    return;
                }
            }
            for child in object.values_mut() {
                rewrite_materialized_file_inputs(child, replacements);
            }
        }
        _ => {}
    }
}

pub fn file_artifact_ids(body: &Value) -> Vec<String> {
    let mut ids = BTreeSet::new();
    collect_artifact_file_ids(body, &mut ids);
    ids.into_iter().collect()
}

pub fn request_has_file(body: &Value) -> bool {
    let mut found = false;
    scan_file_request(body, &mut found);
    found
}

pub fn route_supports_file(capabilities: &[String]) -> bool {
    capabilities.iter().any(|capability| {
        matches!(
            normalize_capability(capability).as_str(),
            "file" | "files" | "file_input" | "document" | "documents" | "native_file_upload" | "file_upload"
        )
    })
}

pub fn native_file_mime_supported(mime: &str) -> bool {
    matches!(
        mime,
        "application/pdf"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    )
}

pub fn is_extractable_text_mime(mime: &str) -> bool {
    matches!(
        mime,
        "text/plain" | "text/markdown" | "text/csv" | "application/json"
    )
}

fn ensure_supported_document(record: &ArtifactRecord) -> Result<(), FileAttachmentError> {
    if is_extractable_text_mime(&record.mime_type) || native_file_mime_supported(&record.mime_type) {
        Ok(())
    } else {
        Err(FileAttachmentError::UnsupportedMime(record.mime_type.clone()))
    }
}

fn extract_text(record: &ArtifactRecord, bytes: &[u8]) -> Result<String, FileAttachmentError> {
    if bytes.len() > MAX_EXTRACTED_FILE_BYTES {
        return Err(FileAttachmentError::ExtractionTooLarge(record.id.clone()));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| FileAttachmentError::InvalidText(record.id.clone()))?;
    if record.mime_type == "application/json" {
        serde_json::from_str::<Value>(text).map_err(|error| {
            FileAttachmentError::Invalid(format!(
                "JSON file '{}' is invalid: {error}",
                record.filename
            ))
        })?;
    }
    let mut chars = text.chars();
    let extracted = chars.by_ref().take(MAX_EXTRACTED_CHARS).collect::<String>();
    if chars.next().is_some() {
        return Err(FileAttachmentError::ExtractionTooLarge(record.id.clone()));
    }
    Ok(extracted)
}

fn collect_file_sources(value: &Value, out: &mut BTreeSet<FileSource>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_file_sources(item, out);
            }
        }
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "file" | "input_file" | "document") {
                if let Some(file_id) = object
                    .get("file_id")
                    .or_else(|| object.get("artifact_id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                {
                    if file_id.starts_with(ARTIFACT_FILE_SCHEME) {
                        out.insert(FileSource::Url(file_id.to_string()));
                    } else {
                        out.insert(FileSource::FileId(file_id.to_string()));
                    }
                }
                if let Some(url) = object
                    .get("file_url")
                    .or_else(|| object.get("url"))
                    .and_then(Value::as_str)
                    .filter(|url| !url.trim().is_empty())
                {
                    out.insert(FileSource::Url(url.to_string()));
                }
            }
            for child in object.values() {
                collect_file_sources(child, out);
            }
        }
        _ => {}
    }
}

fn rewrite_file_sources(
    value: &mut Value,
    replacements: &BTreeMap<String, (String, String, String)>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                rewrite_file_sources(item, replacements);
            }
        }
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "file" | "input_file" | "document") {
                let file_id = object
                    .get("file_id")
                    .or_else(|| object.get("artifact_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let file_url = object
                    .get("file_url")
                    .or_else(|| object.get("url"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let lookup = file_id
                    .as_ref()
                    .map(|id| {
                        if id.starts_with(ARTIFACT_FILE_SCHEME) {
                            format!("url:{id}")
                        } else {
                            format!("file:{id}")
                        }
                    })
                    .or_else(|| file_url.as_ref().map(|url| format!("url:{url}")));
                if let Some((uri, filename, mime_type)) =
                    lookup.and_then(|key| replacements.get(&key)).cloned()
                {
                    object.clear();
                    object.insert("type".into(), Value::String("input_file".into()));
                    object.insert("file_id".into(), Value::String(uri));
                    object.insert("filename".into(), Value::String(filename));
                    object.insert("mime_type".into(), Value::String(mime_type));
                }
            }
            for child in object.values_mut() {
                rewrite_file_sources(child, replacements);
            }
        }
        _ => {}
    }
}

fn scan_file_request(value: &Value, found: &mut bool) {
    if *found {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                scan_file_request(item, found);
            }
        }
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "file" | "input_file" | "document")
                || object.get("file_data").is_some()
            {
                *found = true;
                return;
            }
            for child in object.values() {
                scan_file_request(child, found);
            }
        }
        _ => {}
    }
}

fn collect_artifact_file_ids(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_artifact_file_ids(item, out);
            }
        }
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "file" | "input_file" | "document") {
                if let Some(raw) = object
                    .get("file_id")
                    .or_else(|| object.get("artifact_id"))
                    .or_else(|| object.get("file_url"))
                    .and_then(Value::as_str)
                {
                    let id = raw.strip_prefix(ARTIFACT_FILE_SCHEME).unwrap_or(raw);
                    if id.starts_with("file_") {
                        out.insert(id.to_string());
                    }
                }
                if let Some(id) = object
                    .get("llmgateway_artifact_id")
                    .and_then(Value::as_str)
                    .filter(|id| id.starts_with("file_"))
                {
                    out.insert(id.to_string());
                }
            }
            for child in object.values() {
                collect_artifact_file_ids(child, out);
            }
        }
        _ => {}
    }
}

fn source_key(source: &FileSource) -> String {
    match source {
        FileSource::FileId(id) => format!("file:{id}"),
        FileSource::Url(url) => format!("url:{url}"),
    }
}

fn normalize_capability(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_")
}

fn truncate(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_capabilities_and_mime_strategy_are_deterministic() {
        assert!(route_supports_file(&["native-file-upload".into()]));
        assert!(route_supports_file(&["document".into()]));
        assert!(!route_supports_file(&["vision".into()]));
        assert!(is_extractable_text_mime("application/json"));
        assert!(native_file_mime_supported("application/pdf"));
    }

    fn extraction_record(mime_type: &str) -> ArtifactRecord {
        ArtifactRecord {
            id: "file_guard".into(),
            filename: "guard.txt".into(),
            mime_type: mime_type.into(),
            size_bytes: 0,
            sha256: "sha".into(),
            purpose: "assistants".into(),
            source: "test".into(),
            lifecycle_state: "active".into(),
            created_at: "now".into(),
        }
    }

    #[test]
    fn extraction_guardrails_reject_oversize_bytes_and_characters() {
        let record = extraction_record("text/plain");
        let too_many_bytes = vec![b'a'; MAX_EXTRACTED_FILE_BYTES + 1];
        assert!(matches!(
            extract_text(&record, &too_many_bytes),
            Err(FileAttachmentError::ExtractionTooLarge(id)) if id == "file_guard"
        ));

        let too_many_characters = "é".repeat(MAX_EXTRACTED_CHARS + 1);
        assert!(too_many_characters.len() <= MAX_EXTRACTED_FILE_BYTES);
        assert!(matches!(
            extract_text(&record, too_many_characters.as_bytes()),
            Err(FileAttachmentError::ExtractionTooLarge(id)) if id == "file_guard"
        ));
    }

    #[test]
    fn json_extraction_rejects_invalid_documents() {
        let record = extraction_record("application/json");
        assert!(matches!(
            extract_text(&record, br#"{"broken":"#),
            Err(FileAttachmentError::Invalid(message))
                if message.contains("JSON file 'guard.txt' is invalid")
        ));
    }

    #[test]
    fn file_artifact_collection_does_not_capture_image_uris() {
        let body = json!({
            "messages":[{
                "content":[
                    {"type":"image_url","image_url":{"url":"llmgateway://artifact/file_image"}},
                    {"type":"input_file","file_id":"llmgateway://artifact/file_doc"}
                ]
            }]
        });
        assert_eq!(file_artifact_ids(&body), vec!["file_doc".to_string()]);
    }

    #[test]
    fn execution_strategy_is_bounded_to_known_values() {
        assert_eq!(
            execution_strategy(&json!({"llmgateway_attachment_strategy":"native_upload"})),
            Some("native_upload")
        );
        assert_eq!(
            execution_strategy(&json!({"llmgateway_attachment_strategy":"spoofed"})),
            None
        );
    }

    #[test]
    fn file_request_detection_finds_native_materialized_parts() {
        let body = json!({
            "messages":[{
                "role":"user",
                "content":[{"type":"input_file","file_data":"data:application/pdf;base64,AA=="}]
            }]
        });
        assert!(request_has_file(&body));
    }
}
