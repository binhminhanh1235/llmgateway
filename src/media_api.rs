use crate::{
    api::{
        authorize_client, client_policy_error, json_error, json_response, response_with_route,
        AppState,
    },
    config::{AccountConfig, ProviderConfig, RouteConfig},
};
use axum::{
    body::Body,
    extract::{Multipart, State},
    http::{header::AUTHORIZATION, HeaderMap, HeaderValue, Response, StatusCode},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use reqwest::multipart::{Form, Part};
use serde_json::{json, Value};
use std::{env, sync::Arc, time::Instant};
use uuid::Uuid;

const DEFAULT_IMAGE_COUNT: usize = 1;
const MAX_IMAGE_COUNT: usize = 8;

struct MediaRoute {
    route: RouteConfig,
    account: AccountConfig,
    provider: ProviderConfig,
}

#[derive(Clone)]
struct GeneratedImage {
    file_id: String,
    mime_type: String,
    revised_prompt: Option<String>,
}

#[derive(Default)]
struct TranscriptionInput {
    file_name: String,
    mime_type: String,
    bytes: Vec<u8>,
    model: Option<String>,
    language: Option<String>,
    prompt: Option<String>,
    response_format: Option<String>,
    temperature: Option<String>,
}

#[derive(Default)]
struct ImageEditInput {
    file_name: String,
    mime_type: String,
    bytes: Vec<u8>,
    prompt: String,
    model: Option<String>,
    size: Option<String>,
    quality: Option<String>,
    n: Option<usize>,
}

pub fn response_requests_image_output(body: &Value) -> bool {
    ["modalities", "output_modalities"].iter().any(|field| {
        body.get(*field)
            .and_then(Value::as_array)
            .is_some_and(|values| {
                values
                    .iter()
                    .any(|value| value.as_str().is_some_and(|value| value == "image"))
            })
    })
}

pub async fn responses_image_output(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };
    let config = state.gateway.config_snapshot();
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&config.api.default_model)
        .to_string();
    if let Err(error) = state
        .client_policies
        .enforce_model(&access, &requested_model)
    {
        return client_policy_error(error);
    }
    let Some(prompt) = responses_prompt(&body) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "image output requires text input",
        );
    };
    let n = body
        .get("n")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(DEFAULT_IMAGE_COUNT)
        .clamp(1, MAX_IMAGE_COUNT);

    let generated = match generate_images(
        &state,
        access.policy(),
        access.client_id(),
        &requested_model,
        &prompt,
        n,
        body.get("size").and_then(Value::as_str),
        body.get("quality").and_then(Value::as_str),
    )
    .await
    {
        Ok(value) => value,
        Err(response) => return response,
    };

    let response_id = format!("resp_{}", Uuid::new_v4().simple());
    let output = generated
        .images
        .iter()
        .map(|image| {
            json!({
                "type":"output_image",
                "file_id":image.file_id,
                "url":format!("/v1/files/{}/content", image.file_id),
                "mime_type":image.mime_type,
                "revised_prompt":image.revised_prompt
            })
        })
        .collect::<Vec<_>>();
    let response = json!({
        "id":response_id,
        "object":"response",
        "created_at":Utc::now().timestamp(),
        "status":"completed",
        "model":requested_model,
        "output":output,
        "usage":Value::Null
    });

    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        let created = json!({"type":"response.created","response":{"id":response_id,"status":"in_progress","model":requested_model}});
        let completed = json!({"type":"response.completed","response":response});
        let payload = format!("data: {created}\n\ndata: {completed}\n\ndata: [DONE]\n\n");
        return response_with_route(
            StatusCode::OK,
            "text/event-stream",
            Body::from(payload),
            &generated.route_id,
        );
    }

    let mut response = json_response(StatusCode::OK, response, Some(&generated.route_id));
    response.headers_mut().insert(
        "x-llmgateway-media-task",
        HeaderValue::from_static("image_generation"),
    );
    response
}

pub async fn image_generations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };
    let config = state.gateway.config_snapshot();
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&config.api.default_model)
        .to_string();
    if let Err(error) = state
        .client_policies
        .enforce_model(&access, &requested_model)
    {
        return client_policy_error(error);
    }
    let Some(prompt) = body
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "'prompt' is required",
        );
    };
    let n = body
        .get("n")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(DEFAULT_IMAGE_COUNT);
    if !(1..=MAX_IMAGE_COUNT).contains(&n) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "'n' must be between 1 and 8",
        );
    }

    let generated = match generate_images(
        &state,
        access.policy(),
        access.client_id(),
        &requested_model,
        prompt,
        n,
        body.get("size").and_then(Value::as_str),
        body.get("quality").and_then(Value::as_str),
    )
    .await
    {
        Ok(value) => value,
        Err(response) => return response,
    };
    image_api_response(generated)
}

pub async fn image_edits(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };
    let mut input = ImageEditInput::default();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "image" {
            input.file_name = field.file_name().unwrap_or("image.png").to_string();
            input.mime_type = field.content_type().unwrap_or("image/png").to_string();
            match field.bytes().await {
                Ok(bytes) => input.bytes = bytes.to_vec(),
                Err(error) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("failed to read image: {error}"),
                    )
                }
            }
            continue;
        }
        let value = match field.text().await {
            Ok(value) => value,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("failed to read multipart field '{name}': {error}"),
                )
            }
        };
        match name.as_str() {
            "prompt" => input.prompt = value,
            "model" => input.model = Some(value),
            "size" => input.size = Some(value),
            "quality" => input.quality = Some(value),
            "n" => input.n = value.parse().ok(),
            _ => {}
        }
    }
    if input.bytes.is_empty() || input.prompt.trim().is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "'image' and 'prompt' are required",
        );
    }
    if !input.mime_type.starts_with("image/") {
        return json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "image edit input must use an image MIME type",
        );
    }
    let n = input.n.unwrap_or(DEFAULT_IMAGE_COUNT);
    if !(1..=MAX_IMAGE_COUNT).contains(&n) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "'n' must be between 1 and 8",
        );
    }

    if let Err(error) = state
        .artifacts
        .store_bytes(
            access.client_id(),
            &input.file_name,
            Some(&input.mime_type),
            "image_edit",
            "image_edit_input",
            &input.bytes,
        )
        .await
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "artifact_error",
            &error.to_string(),
        );
    }

    let config = state.gateway.config_snapshot();
    let requested_model = input
        .model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&config.api.default_model)
        .to_string();
    if let Err(error) = state
        .client_policies
        .enforce_model(&access, &requested_model)
    {
        return client_policy_error(error);
    }

    let routes =
        match media_routes(&state, access.policy(), &requested_model, "image_editing").await {
            Ok(routes) => routes,
            Err(response) => return response,
        };
    let started = Instant::now();
    let mut last_error = String::new();
    for candidate in routes {
        if candidate.provider.kind != "openai-compatible" {
            continue;
        }
        let mut form = Form::new()
            .part(
                "image",
                Part::bytes(input.bytes.clone())
                    .file_name(input.file_name.clone())
                    .mime_str(&input.mime_type)
                    .unwrap_or_else(|_| {
                        Part::bytes(input.bytes.clone()).file_name(input.file_name.clone())
                    }),
            )
            .text("prompt", input.prompt.clone())
            .text("model", candidate.route.model.clone())
            .text("n", n.to_string())
            .text("response_format", "b64_json");
        if let Some(size) = &input.size {
            form = form.text("size", size.clone());
        }
        if let Some(quality) = &input.quality {
            form = form.text("quality", quality.clone());
        }
        let url = format!(
            "{}/images/edits",
            candidate.provider.base_url.trim_end_matches('/')
        );
        let response = send_multipart(&candidate.account, &url, form).await;
        match response {
            Ok(response) if response.status().is_success() => {
                let route_id = candidate.route.id.clone();
                state
                    .gateway
                    .router
                    .mark_success(&route_id, elapsed_ms(started))
                    .await;
                let value = match response.json::<Value>().await {
                    Ok(value) => value,
                    Err(error) => {
                        return json_error(
                            StatusCode::BAD_GATEWAY,
                            "upstream_error",
                            &error.to_string(),
                        )
                    }
                };
                let images = match persist_images(
                    &state,
                    access.client_id(),
                    &value,
                    "image_edit",
                    "generated-image.png",
                )
                .await
                {
                    Ok(images) => images,
                    Err(response) => return response,
                };
                return image_api_response(MediaGeneration { route_id, images });
            }
            Ok(response) => {
                let status = response.status();
                last_error = response.text().await.unwrap_or_default();
                state
                    .gateway
                    .router
                    .mark_failure(
                        &candidate.route.id,
                        last_error.clone(),
                        0,
                        elapsed_ms(started),
                        status.is_server_error(),
                    )
                    .await;
                if !retryable_status(status) {
                    return json_error(status, "upstream_error", &last_error);
                }
            }
            Err(error) => {
                last_error = error;
                state
                    .gateway
                    .router
                    .mark_failure(
                        &candidate.route.id,
                        last_error.clone(),
                        2,
                        elapsed_ms(started),
                        true,
                    )
                    .await;
            }
        }
    }
    json_error(
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        if last_error.is_empty() {
            "no image-edit route could be executed"
        } else {
            &last_error
        },
    )
}

pub async fn audio_transcriptions(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response<Body> {
    let access = match authorize_client(&headers, &state) {
        Ok(access) => access,
        Err(response) => return response,
    };
    let mut input = TranscriptionInput::default();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            input.file_name = field.file_name().unwrap_or("audio.webm").to_string();
            input.mime_type = field
                .content_type()
                .unwrap_or("application/octet-stream")
                .to_string();
            match field.bytes().await {
                Ok(bytes) => input.bytes = bytes.to_vec(),
                Err(error) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("failed to read audio: {error}"),
                    )
                }
            }
            continue;
        }
        let value = match field.text().await {
            Ok(value) => value,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("failed to read multipart field '{name}': {error}"),
                )
            }
        };
        match name.as_str() {
            "model" => input.model = Some(value),
            "language" => input.language = Some(value),
            "prompt" => input.prompt = Some(value),
            "response_format" => input.response_format = Some(value),
            "temperature" => input.temperature = Some(value),
            _ => {}
        }
    }
    if input.bytes.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "'file' is required",
        );
    }
    if !input.mime_type.starts_with("audio/") {
        return json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "transcription input must use an audio MIME type",
        );
    }
    if let Err(error) = state
        .artifacts
        .store_bytes(
            access.client_id(),
            &input.file_name,
            Some(&input.mime_type),
            "transcription",
            "audio_input",
            &input.bytes,
        )
        .await
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "artifact_error",
            &error.to_string(),
        );
    }

    let config = state.gateway.config_snapshot();
    let requested_model = input
        .model
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&config.api.default_model)
        .to_string();
    if let Err(error) = state
        .client_policies
        .enforce_model(&access, &requested_model)
    {
        return client_policy_error(error);
    }
    let routes = match media_routes(
        &state,
        access.policy(),
        &requested_model,
        "audio_transcription",
    )
    .await
    {
        Ok(routes) => routes,
        Err(response) => return response,
    };
    let started = Instant::now();
    let mut last_error = String::new();
    for candidate in routes {
        if candidate.provider.kind != "openai-compatible" {
            continue;
        }
        let part = match Part::bytes(input.bytes.clone())
            .file_name(input.file_name.clone())
            .mime_str(&input.mime_type)
        {
            Ok(part) => part,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &error.to_string(),
                )
            }
        };
        let mut form = Form::new()
            .part("file", part)
            .text("model", candidate.route.model.clone());
        if let Some(value) = &input.language {
            form = form.text("language", value.clone());
        }
        if let Some(value) = &input.prompt {
            form = form.text("prompt", value.clone());
        }
        if let Some(value) = &input.response_format {
            form = form.text("response_format", value.clone());
        }
        if let Some(value) = &input.temperature {
            form = form.text("temperature", value.clone());
        }
        let url = format!(
            "{}/audio/transcriptions",
            candidate.provider.base_url.trim_end_matches('/')
        );
        match send_multipart(&candidate.account, &url, form).await {
            Ok(response) if response.status().is_success() => {
                let route_id = candidate.route.id.clone();
                state
                    .gateway
                    .router
                    .mark_success(&route_id, elapsed_ms(started))
                    .await;
                let status = response.status();
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("application/json")
                    .to_string();
                let bytes = match response.bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        return json_error(
                            StatusCode::BAD_GATEWAY,
                            "upstream_error",
                            &error.to_string(),
                        )
                    }
                };
                return response_with_route(status, &content_type, Body::from(bytes), &route_id);
            }
            Ok(response) => {
                let status = response.status();
                last_error = response.text().await.unwrap_or_default();
                state
                    .gateway
                    .router
                    .mark_failure(
                        &candidate.route.id,
                        last_error.clone(),
                        0,
                        elapsed_ms(started),
                        status.is_server_error(),
                    )
                    .await;
                if !retryable_status(status) {
                    return json_error(status, "upstream_error", &last_error);
                }
            }
            Err(error) => {
                last_error = error;
                state
                    .gateway
                    .router
                    .mark_failure(
                        &candidate.route.id,
                        last_error.clone(),
                        2,
                        elapsed_ms(started),
                        true,
                    )
                    .await;
            }
        }
    }
    json_error(
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        if last_error.is_empty() {
            "no transcription route could be executed"
        } else {
            &last_error
        },
    )
}

struct MediaGeneration {
    route_id: String,
    images: Vec<GeneratedImage>,
}

async fn generate_images(
    state: &AppState,
    policy: Option<&crate::config::ClientPolicyConfig>,
    client_id: Option<&str>,
    requested_model: &str,
    prompt: &str,
    n: usize,
    size: Option<&str>,
    quality: Option<&str>,
) -> Result<MediaGeneration, Response<Body>> {
    let routes = media_routes(state, policy, requested_model, "image_generation").await?;
    let started = Instant::now();
    let mut last_error = String::new();
    for candidate in routes {
        if candidate.provider.kind != "openai-compatible" {
            continue;
        }
        let mut upstream = json!({
            "model":candidate.route.model,
            "prompt":prompt,
            "n":n,
            "response_format":"b64_json"
        });
        if let Some(size) = size {
            upstream["size"] = Value::String(size.to_string());
        }
        if let Some(quality) = quality {
            upstream["quality"] = Value::String(quality.to_string());
        }
        let url = format!(
            "{}/images/generations",
            candidate.provider.base_url.trim_end_matches('/')
        );
        match send_json(&candidate.account, &url, &upstream).await {
            Ok(response) if response.status().is_success() => {
                let route_id = candidate.route.id.clone();
                state
                    .gateway
                    .router
                    .mark_success(&route_id, elapsed_ms(started))
                    .await;
                let value = match response.json::<Value>().await {
                    Ok(value) => value,
                    Err(error) => {
                        return Err(json_error(
                            StatusCode::BAD_GATEWAY,
                            "upstream_error",
                            &error.to_string(),
                        ))
                    }
                };
                let images = persist_images(
                    state,
                    client_id,
                    &value,
                    "image_generation",
                    "generated-image.png",
                )
                .await?;
                return Ok(MediaGeneration { route_id, images });
            }
            Ok(response) => {
                let status = response.status();
                last_error = response.text().await.unwrap_or_default();
                state
                    .gateway
                    .router
                    .mark_failure(
                        &candidate.route.id,
                        last_error.clone(),
                        0,
                        elapsed_ms(started),
                        status.is_server_error(),
                    )
                    .await;
                if !retryable_status(status) {
                    return Err(json_error(status, "upstream_error", &last_error));
                }
            }
            Err(error) => {
                last_error = error;
                state
                    .gateway
                    .router
                    .mark_failure(
                        &candidate.route.id,
                        last_error.clone(),
                        2,
                        elapsed_ms(started),
                        true,
                    )
                    .await;
            }
        }
    }
    Err(json_error(
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        if last_error.is_empty() {
            "no image-generation route could be executed"
        } else {
            &last_error
        },
    ))
}

async fn media_routes(
    state: &AppState,
    policy: Option<&crate::config::ClientPolicyConfig>,
    requested_model: &str,
    task: &str,
) -> Result<Vec<MediaRoute>, Response<Body>> {
    let body = json!({"model":requested_model,"llmgateway_task":task});
    let base_config = state.gateway.config_snapshot();
    let config = match state
        .gateway
        .effective_request_config(base_config, policy, &body)
    {
        Ok(config) => config,
        Err(error) => return Err(crate::api::gateway_error(error)),
    };
    let mut routes = state
        .gateway
        .router
        .plan_for_body_with_config(config.clone(), requested_model, Some(&body))
        .await;
    if let Some(policy) = policy {
        routes.retain(|route| policy.route_allowed(&route.id));
    }
    if routes.is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_capability",
            &format!("model '{requested_model}' has no route supporting '{task}'"),
        ));
    }
    let candidates = routes
        .into_iter()
        .filter_map(|route| {
            let account = config.account(&route.account)?.clone();
            let provider = config.provider(&account.provider)?.clone();
            Some(MediaRoute {
                route,
                account,
                provider,
            })
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "unsupported_capability",
            &format!("model '{requested_model}' has no executable route supporting '{task}'"),
        ));
    }
    Ok(candidates)
}

async fn persist_images(
    state: &AppState,
    client_id: Option<&str>,
    upstream: &Value,
    source: &str,
    filename: &str,
) -> Result<Vec<GeneratedImage>, Response<Body>> {
    let Some(data) = upstream.get("data").and_then(Value::as_array) else {
        return Err(json_error(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "image provider response is missing data",
        ));
    };
    let mut images = Vec::new();
    for (index, item) in data.iter().enumerate() {
        let Some(encoded) = item.get("b64_json").and_then(Value::as_str) else {
            return Err(json_error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "image provider must return b64_json; remote output URLs are not ingested",
            ));
        };
        let bytes = match STANDARD.decode(encoded) {
            Ok(bytes) if !bytes.is_empty() => bytes,
            Ok(_) => {
                return Err(json_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "image provider returned an empty image",
                ))
            }
            Err(error) => {
                return Err(json_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    &format!("invalid image base64: {error}"),
                ))
            }
        };
        let mime_type = detect_image_mime(&bytes).ok_or_else(|| {
            json_error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "image provider returned unsupported image bytes",
            )
        })?;
        let indexed_filename = if data.len() == 1 {
            filename.to_string()
        } else {
            format!(
                "generated-image-{}.{}",
                index + 1,
                extension_for_mime(mime_type)
            )
        };
        let record = state
            .artifacts
            .store_bytes(
                client_id,
                &indexed_filename,
                Some(mime_type),
                "generated_image",
                source,
                &bytes,
            )
            .await
            .map_err(|error| {
                json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "artifact_error",
                    &error.to_string(),
                )
            })?;
        images.push(GeneratedImage {
            file_id: record.id,
            mime_type: record.mime_type,
            revised_prompt: item
                .get("revised_prompt")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    if images.is_empty() {
        return Err(json_error(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "image provider returned no images",
        ));
    }
    Ok(images)
}

fn image_api_response(generated: MediaGeneration) -> Response<Body> {
    let data = generated
        .images
        .iter()
        .map(|image| {
            json!({
                "file_id":image.file_id,
                "url":format!("/v1/files/{}/content", image.file_id),
                "mime_type":image.mime_type,
                "revised_prompt":image.revised_prompt
            })
        })
        .collect::<Vec<_>>();
    json_response(
        StatusCode::OK,
        json!({"created":Utc::now().timestamp(),"data":data}),
        Some(&generated.route_id),
    )
}

async fn send_json(
    account: &AccountConfig,
    url: &str,
    value: &Value,
) -> Result<reqwest::Response, String> {
    let key = env::var(&account.api_key_env).map_err(|_| {
        format!(
            "missing credential environment variable '{}'",
            account.api_key_env
        )
    })?;
    let client = reqwest::Client::new();
    let request = apply_auth(client.post(url).json(value), account, &key)?;
    request.send().await.map_err(|error| error.to_string())
}

async fn send_multipart(
    account: &AccountConfig,
    url: &str,
    form: Form,
) -> Result<reqwest::Response, String> {
    let key = env::var(&account.api_key_env).map_err(|_| {
        format!(
            "missing credential environment variable '{}'",
            account.api_key_env
        )
    })?;
    let client = reqwest::Client::new();
    let request = apply_auth(client.post(url).multipart(form), account, &key)?;
    request.send().await.map_err(|error| error.to_string())
}

fn apply_auth(
    request: reqwest::RequestBuilder,
    account: &AccountConfig,
    key: &str,
) -> Result<reqwest::RequestBuilder, String> {
    match account.auth_style.as_str() {
        "bearer" => Ok(request.bearer_auth(key)),
        "x-api-key" => Ok(request.header("x-api-key", key)),
        other => Err(format!("unsupported auth_style '{other}'")),
    }
}

fn responses_prompt(body: &Value) -> Option<String> {
    fn collect(value: &Value, output: &mut Vec<String>) {
        match value {
            Value::String(text) if !text.trim().is_empty() => output.push(text.clone()),
            Value::Array(values) => {
                for value in values {
                    collect(value, output);
                }
            }
            Value::Object(object) => {
                if object
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| matches!(kind, "input_text" | "text"))
                {
                    if let Some(text) = object.get("text").and_then(Value::as_str) {
                        if !text.trim().is_empty() {
                            output.push(text.to_string());
                        }
                    }
                    return;
                }
                if let Some(content) = object.get("content") {
                    collect(content, output);
                } else if let Some(input) = object.get("input") {
                    collect(input, output);
                }
            }
            _ => {}
        }
    }
    let mut values = Vec::new();
    if let Some(input) = body.get("input") {
        collect(input, &mut values);
    }
    (!values.is_empty()).then(|| values.join("\n"))
}

fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.len() >= 3 && bytes[..3] == [0xff, 0xd8, 0xff] {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    }
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 429 | 500 | 502 | 503 | 504)
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::{detect_image_mime, response_requests_image_output, responses_prompt};
    use serde_json::json;

    #[test]
    fn responses_image_output_detection_is_explicit() {
        assert!(response_requests_image_output(&json!({
            "output_modalities":["text","image"]
        })));
        assert!(!response_requests_image_output(&json!({
            "output_modalities":["text"]
        })));
    }

    #[test]
    fn responses_prompt_collects_text_without_attachment_ids() {
        let prompt = responses_prompt(&json!({
            "input":[{
                "role":"user",
                "content":[
                    {"type":"input_text","text":"draw a sunrise"},
                    {"type":"input_image","file_id":"file_secret"}
                ]
            }]
        }))
        .unwrap();
        assert_eq!(prompt, "draw a sunrise");
        assert!(!prompt.contains("file_secret"));
    }

    #[test]
    fn generated_image_bytes_must_have_supported_signature() {
        assert_eq!(
            detect_image_mime(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(detect_image_mime(b"not-an-image"), None);
    }
}
