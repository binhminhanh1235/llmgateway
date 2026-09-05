use crate::artifact_store::{ArtifactError, ArtifactRecord, ArtifactStore};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const ARTIFACT_IMAGE_SCHEME: &str = "llmgateway://artifact/";

#[derive(Debug, Error)]
pub enum VisionError {
    #[error("{0}")]
    Artifact(#[from] ArtifactError),
    #[error("invalid image input: {0}")]
    Invalid(String),
    #[error("unsupported capability '{0}'")]
    UnsupportedCapability(String),
    #[error("artifact '{0}' is not an image")]
    NotImage(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ImageSource {
    FileId(String),
    Url(String),
}

pub async fn resolve_image_inputs(
    body: &mut Value,
    store: &ArtifactStore,
    owner_client_id: Option<&str>,
    admin: bool,
) -> Result<Vec<String>, VisionError> {
    let mut sources = BTreeSet::new();
    collect_image_sources(body, &mut sources);
    let mut replacements = BTreeMap::new();
    let mut artifact_ids = BTreeSet::new();

    for source in sources {
        let (key, record) = match &source {
            ImageSource::FileId(id) => {
                let record = store.get(id, owner_client_id, admin).await?;
                ensure_image(&record)?;
                (source_key(&source), record)
            }
            ImageSource::Url(url) if url.starts_with(ARTIFACT_IMAGE_SCHEME) => {
                let id = url.trim_start_matches(ARTIFACT_IMAGE_SCHEME);
                if id.trim().is_empty() {
                    return Err(VisionError::Invalid(
                        "artifact image URL is missing a file id".into(),
                    ));
                }
                let record = store.get(id, owner_client_id, admin).await?;
                ensure_image(&record)?;
                (source_key(&source), record)
            }
            ImageSource::Url(url) if url.starts_with("data:") => {
                let (mime, bytes) = decode_data_url(url)?;
                if !mime.starts_with("image/") {
                    return Err(VisionError::NotImage(mime));
                }
                let extension = image_extension(&mime);
                let record = store
                    .store_bytes(
                        owner_client_id,
                        &format!("inline-image.{extension}"),
                        Some(&mime),
                        "vision",
                        "inline_data_url",
                        &bytes,
                    )
                    .await?;
                (source_key(&source), record)
            }
            ImageSource::Url(url) if url.starts_with("http://") || url.starts_with("https://") => {
                return Err(VisionError::UnsupportedCapability(
                    "remote_image_url".into(),
                ));
            }
            ImageSource::Url(url) if url.starts_with("file_") => {
                let record = store.get(url, owner_client_id, admin).await?;
                ensure_image(&record)?;
                (source_key(&source), record)
            }
            ImageSource::Url(url) => {
                return Err(VisionError::Invalid(format!(
                    "image URL must be a data URL, file id, or {ARTIFACT_IMAGE_SCHEME} reference; got '{}'",
                    truncate(url, 96)
                )));
            }
        };
        artifact_ids.insert(record.id.clone());
        replacements.insert(key, format!("{ARTIFACT_IMAGE_SCHEME}{}", record.id));
    }

    rewrite_image_sources(body, &replacements);
    Ok(artifact_ids.into_iter().collect())
}

pub async fn materialize_image_inputs(
    body: &Value,
    store: &ArtifactStore,
    owner_client_id: Option<&str>,
    admin: bool,
) -> Result<Value, VisionError> {
    let mut materialized = body.clone();
    let mut ids = BTreeSet::new();
    collect_artifact_image_ids(&materialized, &mut ids);
    let mut replacements = BTreeMap::new();
    for id in ids {
        let (record, bytes) = store.read_content(&id, owner_client_id, admin).await?;
        ensure_image(&record)?;
        replacements.insert(
            format!("{ARTIFACT_IMAGE_SCHEME}{id}"),
            format!("data:{};base64,{}", record.mime_type, STANDARD.encode(bytes)),
        );
    }
    rewrite_artifact_urls(&mut materialized, &replacements);
    Ok(materialized)
}

pub fn image_artifact_ids(body: &Value) -> Vec<String> {
    let mut ids = BTreeSet::new();
    collect_artifact_image_ids(body, &mut ids);
    ids.into_iter().collect()
}

pub fn request_has_image(body: &Value) -> bool {
    let mut sources = BTreeSet::new();
    collect_image_sources(body, &mut sources);
    !sources.is_empty()
}

pub fn route_supports_image(capabilities: &[String]) -> bool {
    capabilities.iter().any(|capability| {
        matches!(
            normalize_capability(capability).as_str(),
            "vision" | "image" | "image_input"
        )
    })
}

fn collect_image_sources(value: &Value, out: &mut BTreeSet<ImageSource>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_image_sources(item, out);
            }
        }
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "image_url" | "input_image" | "image") {
                if let Some(file_id) = object
                    .get("file_id")
                    .or_else(|| object.get("artifact_id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                {
                    out.insert(ImageSource::FileId(file_id.to_string()));
                }
                if let Some(image_url) = object.get("image_url") {
                    if let Some(url) = image_url.as_str() {
                        if !url.trim().is_empty() {
                            out.insert(ImageSource::Url(url.to_string()));
                        }
                    } else if let Some(url) = image_url
                        .get("url")
                        .and_then(Value::as_str)
                        .filter(|url| !url.trim().is_empty())
                    {
                        out.insert(ImageSource::Url(url.to_string()));
                    }
                }
                if let Some(url) = object
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|url| !url.trim().is_empty())
                {
                    out.insert(ImageSource::Url(url.to_string()));
                }
                if let Some(source) = object.get("source") {
                    if source.get("type").and_then(Value::as_str) == Some("base64") {
                        if let (Some(mime), Some(data)) = (
                            source.get("media_type").and_then(Value::as_str),
                            source.get("data").and_then(Value::as_str),
                        ) {
                            out.insert(ImageSource::Url(format!(
                                "data:{mime};base64,{data}"
                            )));
                        }
                    }
                }
            }
            for child in object.values() {
                collect_image_sources(child, out);
            }
        }
        _ => {}
    }
}

fn rewrite_image_sources(value: &mut Value, replacements: &BTreeMap<String, String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                rewrite_image_sources(item, replacements);
            }
        }
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "image_url" | "input_image" | "image") {
                let file_id = object
                    .get("file_id")
                    .or_else(|| object.get("artifact_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let direct_url = object
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let nested_url = object
                    .get("image_url")
                    .and_then(|image_url| {
                        image_url
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| image_url.get("url").and_then(Value::as_str).map(str::to_string))
                    });
                let source_data = object.get("source").and_then(|source| {
                    if source.get("type").and_then(Value::as_str) != Some("base64") {
                        return None;
                    }
                    Some(format!(
                        "data:{};base64,{}",
                        source.get("media_type").and_then(Value::as_str)?,
                        source.get("data").and_then(Value::as_str)?
                    ))
                });
                let lookup = file_id
                    .as_ref()
                    .map(|id| format!("file:{id}"))
                    .or_else(|| nested_url.as_ref().map(|url| format!("url:{url}")))
                    .or_else(|| direct_url.as_ref().map(|url| format!("url:{url}")))
                    .or_else(|| source_data.as_ref().map(|url| format!("url:{url}")));
                if let Some(uri) = lookup.and_then(|key| replacements.get(&key)).cloned() {
                    object.remove("file_id");
                    object.remove("artifact_id");
                    object.remove("source");
                    object.remove("url");
                    object.insert("type".into(), Value::String("image_url".into()));
                    object.insert(
                        "image_url".into(),
                        serde_json::json!({"url":uri}),
                    );
                }
            }
            for child in object.values_mut() {
                rewrite_image_sources(child, replacements);
            }
        }
        _ => {}
    }
}

fn collect_artifact_image_ids(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::String(raw) if raw.starts_with(ARTIFACT_IMAGE_SCHEME) => {
            let id = raw.trim_start_matches(ARTIFACT_IMAGE_SCHEME);
            if !id.is_empty() {
                out.insert(id.to_string());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_artifact_image_ids(item, out);
            }
        }
        Value::Object(object) => {
            for child in object.values() {
                collect_artifact_image_ids(child, out);
            }
        }
        _ => {}
    }
}

fn rewrite_artifact_urls(value: &mut Value, replacements: &BTreeMap<String, String>) {
    match value {
        Value::String(raw) => {
            if let Some(replacement) = replacements.get(raw) {
                *raw = replacement.clone();
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_artifact_urls(item, replacements);
            }
        }
        Value::Object(object) => {
            for child in object.values_mut() {
                rewrite_artifact_urls(child, replacements);
            }
        }
        _ => {}
    }
}

fn source_key(source: &ImageSource) -> String {
    match source {
        ImageSource::FileId(id) => format!("file:{id}"),
        ImageSource::Url(url) => format!("url:{url}"),
    }
}

fn ensure_image(record: &ArtifactRecord) -> Result<(), VisionError> {
    if record.mime_type.starts_with("image/") {
        Ok(())
    } else {
        Err(VisionError::NotImage(record.id.clone()))
    }
}

fn decode_data_url(raw: &str) -> Result<(String, Vec<u8>), VisionError> {
    let rest = raw
        .strip_prefix("data:")
        .ok_or_else(|| VisionError::Invalid("image data URL must start with data:".into()))?;
    let (metadata, encoded) = rest
        .split_once(',')
        .ok_or_else(|| VisionError::Invalid("image data URL is missing ','".into()))?;
    let mut segments = metadata.split(';');
    let mime = segments.next().unwrap_or("").trim().to_ascii_lowercase();
    if mime.is_empty() || !mime.starts_with("image/") {
        return Err(VisionError::NotImage(mime));
    }
    if !segments.any(|segment| segment.eq_ignore_ascii_case("base64")) {
        return Err(VisionError::Invalid(
            "image data URLs must use base64 encoding".into(),
        ));
    }
    let bytes = STANDARD
        .decode(encoded.as_bytes())
        .map_err(|error| VisionError::Invalid(format!("invalid base64 image data: {error}")))?;
    Ok((mime, bytes))
}

fn image_extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
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
    fn image_detection_and_capability_tags_are_deterministic() {
        let body = serde_json::json!({
            "messages":[{
                "role":"user",
                "content":[
                    {"type":"text","text":"describe"},
                    {"type":"image_url","image_url":{"url":"llmgateway://artifact/file_abc"}}
                ]
            }]
        });
        assert!(request_has_image(&body));
        assert_eq!(image_artifact_ids(&body), vec!["file_abc".to_string()]);
        assert!(route_supports_image(&["chat".into(), "vision".into()]));
        assert!(route_supports_image(&["image-input".into()]));
        assert!(!route_supports_image(&["chat".into(), "streaming".into()]));
    }

    #[test]
    fn data_url_decoder_rejects_non_image_and_non_base64() {
        assert!(matches!(
            decode_data_url("data:text/plain;base64,SGk="),
            Err(VisionError::NotImage(_))
        ));
        assert!(matches!(
            decode_data_url("data:image/png,raw"),
            Err(VisionError::Invalid(_))
        ));
    }
}
