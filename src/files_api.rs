use crate::{
    api::{authorize_client, json_error, json_response, AppState},
    artifact_store::{ArtifactError, ArtifactRecord},
};
use axum::{
    body::Body,
    extract::{Multipart, Path, State},
    http::{
        header::{CONTENT_DISPOSITION, CONTENT_TYPE},
        HeaderMap, HeaderValue, Response, StatusCode,
    },
};
use chrono::DateTime;
use serde_json::json;

pub async fn upload_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };
    let limits = state.artifacts.config().clone();
    let mut total_bytes = 0usize;
    let mut file_bytes = Vec::new();
    let mut filename = None;
    let mut declared_mime = None;
    let mut purpose = "assistants".to_string();
    let mut file_count = 0usize;

    loop {
        let next = match multipart.next_field().await {
            Ok(next) => next,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("invalid multipart upload: {error}"),
                )
            }
        };
        let Some(mut field) = next else { break };
        let field_name = field.name().unwrap_or("").to_string();

        if field_name == "file" {
            file_count += 1;
            if file_count > limits.max_files_per_request {
                return json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "file_limit_exceeded",
                    "multipart upload contains too many files",
                );
            }
            if file_count > 1 {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "POST /v1/files accepts exactly one file per request",
                );
            }
            filename = field.file_name().map(str::to_string);
            declared_mime = field.content_type().map(str::to_string);
            while let Some(chunk) = match field.chunk().await {
                Ok(chunk) => chunk,
                Err(error) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("failed to read multipart file: {error}"),
                    )
                }
            } {
                total_bytes = match total_bytes.checked_add(chunk.len()) {
                    Some(total) => total,
                    None => {
                        return json_error(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "file_limit_exceeded",
                            "multipart request size overflow",
                        )
                    }
                };
                if total_bytes > limits.max_request_size_bytes {
                    return json_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request_too_large",
                        "multipart request exceeds configured total size limit",
                    );
                }
                if file_bytes.len().saturating_add(chunk.len()) > limits.max_file_size_bytes {
                    return json_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "file_too_large",
                        "file exceeds configured per-file size limit",
                    );
                }
                file_bytes.extend_from_slice(&chunk);
            }
        } else if field_name == "purpose" {
            let bytes = match field.bytes().await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("failed to read purpose field: {error}"),
                    )
                }
            };
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > limits.max_request_size_bytes {
                return json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    "multipart request exceeds configured total size limit",
                );
            }
            let value = String::from_utf8_lossy(&bytes).trim().to_string();
            if value.is_empty() || value.len() > 128 {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "purpose must contain between 1 and 128 UTF-8 bytes",
                );
            }
            purpose = value;
        } else {
            let bytes = match field.bytes().await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("failed to read multipart field: {error}"),
                    )
                }
            };
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > limits.max_request_size_bytes {
                return json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    "multipart request exceeds configured total size limit",
                );
            }
        }
    }

    if file_count != 1 {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "multipart field 'file' is required",
        );
    }
    let Some(filename) = filename else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "uploaded file must include a filename",
        );
    };

    match state
        .artifacts
        .store_bytes(
            access.client_id(),
            &filename,
            declared_mime.as_deref(),
            &purpose,
            "api_upload",
            &file_bytes,
        )
        .await
    {
        Ok(record) => file_response(StatusCode::OK, &record),
        Err(error) => artifact_error(error),
    }
}

pub async fn get_file(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };
    match state
        .artifacts
        .get(&file_id, access.client_id(), access.client_id().is_none())
        .await
    {
        Ok(record) => file_response(StatusCode::OK, &record),
        Err(error) => artifact_error(error),
    }
}

pub async fn get_file_content(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };
    match state
        .artifacts
        .read_content(&file_id, access.client_id(), access.client_id().is_none())
        .await
    {
        Ok((record, bytes)) => {
            let mut response = Response::builder()
                .status(StatusCode::OK)
                .body(Body::from(bytes))
                .expect("valid file content response");
            if let Ok(value) = HeaderValue::from_str(&record.mime_type) {
                response.headers_mut().insert(CONTENT_TYPE, value);
            }
            let disposition = format!(
                "attachment; filename=\"{}\"",
                record.filename.replace('"', "")
            );
            if let Ok(value) = HeaderValue::from_str(&disposition) {
                response.headers_mut().insert(CONTENT_DISPOSITION, value);
            }
            response
        }
        Err(error) => artifact_error(error),
    }
}

pub async fn delete_file(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };
    match state
        .artifacts
        .delete(&file_id, access.client_id(), access.client_id().is_none())
        .await
    {
        Ok(()) => json_response(
            StatusCode::OK,
            json!({"id":file_id,"object":"file","deleted":true}),
            None,
        ),
        Err(error) => artifact_error(error),
    }
}

fn file_response(status: StatusCode, record: &ArtifactRecord) -> Response<Body> {
    let created_at = DateTime::parse_from_rfc3339(&record.created_at)
        .map(|value| value.timestamp())
        .unwrap_or_default();
    json_response(
        status,
        json!({
            "id":record.id,
            "object":"file",
            "bytes":record.size_bytes,
            "created_at":created_at,
            "filename":record.filename,
            "purpose":record.purpose,
            "status":"processed",
            "mime_type":record.mime_type,
            "sha256":record.sha256,
            "llmgateway":{
                "source":record.source,
                "lifecycle_state":record.lifecycle_state
            }
        }),
        None,
    )
}

fn artifact_error(error: ArtifactError) -> Response<Body> {
    match error {
        ArtifactError::Invalid(message) => {
            json_error(StatusCode::BAD_REQUEST, "invalid_request_error", &message)
        }
        ArtifactError::TooLarge { limit } => json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            &format!("file exceeds configured limit of {limit} bytes"),
        ),
        ArtifactError::MimeDenied(mime) => json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            &format!("MIME type '{mime}' is not allowed"),
        ),
        ArtifactError::MimeMismatch { declared, detected } => json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "mime_type_mismatch",
            &format!("declared MIME '{declared}' does not match detected MIME '{detected}'"),
        ),
        ArtifactError::NotFound(_) => json_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "file was not found",
        ),
        ArtifactError::InUse { references, .. } => json_error(
            StatusCode::CONFLICT,
            "artifact_in_use",
            &format!("file is still referenced by {references} persisted object(s)"),
        ),
        ArtifactError::Database(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "artifact_database_error",
            &error.to_string(),
        ),
        ArtifactError::Io(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "artifact_storage_error",
            &error.to_string(),
        ),
    }
}
