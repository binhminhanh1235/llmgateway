use crate::{
    api::{authorize_client, AppState},
    local_client::{LocalClientError, LocalGatewayClient},
};
use axum::{
    body::Body,
    extract::State,
    http::{header::CONTENT_TYPE, HeaderMap, HeaderValue, Response, StatusCode},
    Json,
};
use serde_json::{json, Map, Value};
use std::{error::Error, net::IpAddr};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
pub const LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";
const LEGACY_SESSION_ID: &str = "llmgateway-legacy-stateless";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpEra {
    Modern,
    Legacy,
}

pub async fn mcp_http(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(message): Json<Value>,
) -> Response<Body> {
    if let Err(response) = authorize_client(&headers, &state) {
        return response;
    }

    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let version = headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok());

    let era = if method == "initialize" && version.is_none() {
        McpEra::Legacy
    } else {
        match version {
            Some(MODERN_PROTOCOL_VERSION) => McpEra::Modern,
            Some(LEGACY_PROTOCOL_VERSION) => McpEra::Legacy,
            None => McpEra::Modern,
            Some(other) => {
                return mcp_json_response(
                    rpc_error(
                        message.get("id").cloned().unwrap_or(Value::Null),
                        -32022,
                        &format!("unsupported MCP protocol version '{other}'"),
                    ),
                    None,
                )
            }
        }
    };

    if era == McpEra::Modern {
        if let Err(response) = validate_modern_headers(&headers, &message) {
            return response;
        }
    }

    let Some(key) = presented_key(&headers) else {
        return mcp_json_response(
            rpc_error(
                message.get("id").cloned().unwrap_or(Value::Null),
                -32600,
                "missing llmgateway credential",
            ),
            None,
        );
    };

    let base_url = local_base_url(&state);
    let client = match LocalGatewayClient::new(base_url, Some(key.to_string()), None) {
        Ok(client) => client,
        Err(error) => {
            return mcp_json_response(
                rpc_error(
                    message.get("id").cloned().unwrap_or(Value::Null),
                    -32603,
                    &error.to_string(),
                ),
                None,
            )
        }
    };

    let response = dispatch(&client, &message, era).await;
    let session = (era == McpEra::Legacy).then_some(LEGACY_SESSION_ID);
    match response {
        Some(value) => mcp_json_response(value, session),
        None => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap_or_else(|_| Response::new(Body::empty())),
    }
}

pub async fn serve_stdio() -> Result<(), Box<dyn Error>> {
    let client = LocalGatewayClient::from_env()?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut era = McpEra::Modern;

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let message = match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(object)) => Value::Object(object),
            Ok(_) => {
                write_stdio(
                    &mut stdout,
                    &rpc_error(Value::Null, -32600, "Invalid Request"),
                )
                .await?;
                continue;
            }
            Err(_) => {
                write_stdio(&mut stdout, &rpc_error(Value::Null, -32700, "Parse error")).await?;
                continue;
            }
        };

        if message.get("method").and_then(Value::as_str) == Some("initialize") {
            era = McpEra::Legacy;
        } else if message.get("method").and_then(Value::as_str) == Some("server/discover") {
            era = McpEra::Modern;
        } else if request_declares_modern(&message) {
            era = McpEra::Modern;
        }

        if let Some(response) = dispatch(&client, &message, era).await {
            write_stdio(&mut stdout, &response).await?;
        }
    }
    Ok(())
}

async fn write_stdio(
    stdout: &mut tokio::io::Stdout,
    value: &Value,
) -> Result<(), Box<dyn Error>> {
    let mut rendered = serde_json::to_vec(value)?;
    rendered.push(b'\n');
    stdout.write_all(&rendered).await?;
    stdout.flush().await?;
    Ok(())
}

async fn dispatch(
    client: &LocalGatewayClient,
    message: &Value,
    era: McpEra,
) -> Option<Value> {
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(rpc_error(
            message.get("id").cloned().unwrap_or(Value::Null),
            -32600,
            "Invalid Request",
        ));
    }

    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    match method {
        "notifications/initialized" | "notifications/cancelled" => None,
        "server/discover" => Some(rpc_result(
            id,
            complete(
                json!({
                    "supportedVersions":[MODERN_PROTOCOL_VERSION, LEGACY_PROTOCOL_VERSION],
                    "capabilities":{"tools":{}},
                    "serverInfo":{"name":"llmgateway","version":env!("CARGO_PKG_VERSION")},
                    "instructions":"Use capability/resolve tools before provider-specific models. READ + EXECUTE only.",
                    "ttlMs":300000,
                    "cacheScope":"private"
                }),
                McpEra::Modern,
            ),
        )),
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(LEGACY_PROTOCOL_VERSION);
            let negotiated = if requested == LEGACY_PROTOCOL_VERSION {
                LEGACY_PROTOCOL_VERSION
            } else {
                "2025-06-18"
            };
            Some(rpc_result(
                id,
                json!({
                    "protocolVersion":negotiated,
                    "capabilities":{"tools":{"listChanged":false}},
                    "serverInfo":{"name":"llmgateway","version":env!("CARGO_PKG_VERSION")},
                    "instructions":"Use llmgateway_resolve for capability-aware routing. Mutation/admin tools are not exposed."
                }),
            ))
        }
        "ping" => Some(rpc_result(id, complete(json!({}), era))),
        "tools/list" => Some(rpc_result(
            id,
            complete(
                json!({
                    "tools": tool_catalog(),
                    "ttlMs":300000,
                    "cacheScope":"private"
                }),
                era,
            ),
        )),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(rpc_error(id, -32602, "tools/call requires name"));
            };
            let arguments = params
                .get("arguments")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            Some(rpc_result(
                id,
                call_tool(client, name, arguments, era).await,
            ))
        }
        _ => Some(rpc_error(
            id,
            -32601,
            &format!("Method not found: {method}"),
        )),
    }
}

async fn call_tool(
    client: &LocalGatewayClient,
    name: &str,
    arguments: Map<String, Value>,
    era: McpEra,
) -> Value {
    let result = match name {
        "llmgateway_health" => client.health().await,
        "llmgateway_capabilities" => client.capabilities().await,
        "llmgateway_models" => client.models().await,
        "llmgateway_resolve" => {
            client.resolve(false, &route_payload(&arguments)).await
        }
        "llmgateway_diagnostics" => {
            client.resolve(true, &route_payload(&arguments)).await
        }
        "llmgateway_responses" => match execution_payload(&arguments, "responses") {
            Ok(body) => client.responses(&body).await,
            Err(error) => Err(error),
        },
        "llmgateway_chat" => match execution_payload(&arguments, "chat") {
            Ok(body) => client.chat(&body).await,
            Err(error) => Err(error),
        },
        "llmgateway_messages" => match execution_payload(&arguments, "messages") {
            Ok(body) => client.messages(&body).await,
            Err(error) => Err(error),
        },
        _ => {
            return complete(
                json!({
                    "content":[{"type":"text","text":format!("Unknown tool: {name}")}],
                    "isError":true
                }),
                era,
            )
        }
    };

    match result {
        Ok(value) => {
            let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
            complete(
                json!({
                    "content":[{"type":"text","text":text}],
                    "structuredContent":value,
                    "isError":false
                }),
                era,
            )
        }
        Err(error) => complete(
            json!({
                "content":[{"type":"text","text":error.to_string()}],
                "isError":true
            }),
            era,
        ),
    }
}

fn route_payload(arguments: &Map<String, Value>) -> Value {
    let mut payload = Map::new();
    payload.insert(
        "requirements".into(),
        json!({
            "capabilities":arguments.get("capabilities").cloned().unwrap_or_else(|| json!([])),
            "min_context_window":arguments.get("min_context_window").cloned().unwrap_or(Value::Null)
        }),
    );
    for key in ["model", "task", "body"] {
        if let Some(value) = arguments.get(key) {
            payload.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(payload)
}

fn execution_payload(
    arguments: &Map<String, Value>,
    protocol: &str,
) -> Result<Value, LocalClientError> {
    let prompt = arguments
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid_local("prompt must be a non-empty string"))?;
    let default_model = if protocol == "messages" {
        "llmgateway-coding"
    } else {
        "llmgateway-auto"
    };
    let model = arguments
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(default_model);

    let mut body = match protocol {
        "responses" => json!({"model":model,"input":prompt}),
        "messages" => json!({
            "model":model,
            "max_tokens":arguments.get("max_tokens").and_then(Value::as_i64).unwrap_or(1024),
            "messages":[{"role":"user","content":prompt}]
        }),
        _ => json!({
            "model":model,
            "messages":[{"role":"user","content":prompt}]
        }),
    };
    let object = body
        .as_object_mut()
        .expect("MCP execution payload must be an object");
    if let Some(task) = arguments.get("task").cloned() {
        object.insert("llmgateway_task".into(), task);
    }
    let capabilities = arguments
        .get("capabilities")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let min_context_window = arguments
        .get("min_context_window")
        .cloned()
        .unwrap_or(Value::Null);
    if capabilities.as_array().is_some_and(|values| !values.is_empty())
        || !min_context_window.is_null()
    {
        object.insert(
            "llmgateway_requirements".into(),
            json!({
                "capabilities":capabilities,
                "min_context_window":min_context_window
            }),
        );
    }
    Ok(body)
}

fn invalid_local(message: &str) -> LocalClientError {
    LocalClientError::Http {
        status: 400,
        path: "mcp tool arguments".into(),
        body: message.into(),
    }
}

fn complete(mut value: Value, era: McpEra) -> Value {
    if era == McpEra::Modern {
        if let Some(object) = value.as_object_mut() {
            object.insert("resultType".into(), Value::String("complete".into()));
        }
    }
    value
}

fn tool_catalog() -> Vec<Value> {
    vec![
        tool("llmgateway_health", "Check llmgateway health.", json!({"type":"object","properties":{},"additionalProperties":false})),
        tool("llmgateway_capabilities", "List client-visible models and capability metadata.", json!({"type":"object","properties":{},"additionalProperties":false})),
        tool("llmgateway_models", "List compatibility models visible to the current client.", json!({"type":"object","properties":{},"additionalProperties":false})),
        tool("llmgateway_resolve", "Dry-run capability-aware routing through the existing Router.", route_schema()),
        tool("llmgateway_diagnostics", "Return normalized route blockers and recommended next action.", route_schema()),
        tool("llmgateway_responses", "Execute a non-streaming OpenAI Responses request.", execution_schema(false)),
        tool("llmgateway_chat", "Execute a non-streaming OpenAI Chat Completions request.", execution_schema(false)),
        tool("llmgateway_messages", "Execute a non-streaming Anthropic Messages request.", execution_schema(true)),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name":name,
        "description":description,
        "inputSchema":input_schema
    })
}

fn route_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "model":{"type":"string"},
            "task":{"type":"string"},
            "capabilities":{"type":"array","items":{"type":"string"}},
            "min_context_window":{"type":"integer","minimum":1},
            "body":{"type":"object"}
        },
        "additionalProperties":false
    })
}

fn execution_schema(messages: bool) -> Value {
    let mut properties = Map::new();
    properties.insert("model".into(), json!({"type":"string"}));
    properties.insert("prompt".into(), json!({"type":"string"}));
    properties.insert("task".into(), json!({"type":"string"}));
    properties.insert(
        "capabilities".into(),
        json!({"type":"array","items":{"type":"string"}}),
    );
    properties.insert(
        "min_context_window".into(),
        json!({"type":"integer","minimum":1}),
    );
    if messages {
        properties.insert(
            "max_tokens".into(),
            json!({"type":"integer","minimum":1,"default":1024}),
        );
    }
    json!({
        "type":"object",
        "required":["prompt"],
        "properties":properties,
        "additionalProperties":false
    })
}

fn validate_modern_headers(headers: &HeaderMap, message: &Value) -> Result<(), Response<Body>> {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let header_method = headers
        .get("mcp-method")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if header_method != method {
        return Err(mcp_json_response(
            rpc_error(id, -32020, "Mcp-Method header does not match JSON-RPC method"),
            None,
        ));
    }
    if method == "tools/call" {
        let body_name = message
            .get("params")
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let header_name = headers
            .get("mcp-name")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if header_name != body_name {
            return Err(mcp_json_response(
                rpc_error(
                    message.get("id").cloned().unwrap_or(Value::Null),
                    -32020,
                    "Mcp-Name header does not match tool name",
                ),
                None,
            ));
        }
    }
    Ok(())
}

fn request_declares_modern(message: &Value) -> bool {
    message
        .get("params")
        .and_then(|value| value.get("_meta"))
        .and_then(|value| value.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str)
        == Some(MODERN_PROTOCOL_VERSION)
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn mcp_json_response(value: Value, legacy_session: Option<&str>) -> Response<Body> {
    let rendered = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json");
    if let Some(session) = legacy_session {
        builder = builder.header(
            "mcp-session-id",
            HeaderValue::from_str(session).unwrap_or_else(|_| HeaderValue::from_static("llmgateway")),
        );
    }
    builder
        .body(Body::from(rendered))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn presented_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|value| value.to_str().ok()))
}

fn local_base_url(state: &AppState) -> String {
    let config = state.gateway.config_snapshot();
    let host = if config.server.host.is_unspecified() {
        match config.server.host {
            IpAddr::V4(_) => "127.0.0.1".to_string(),
            IpAddr::V6(_) => "::1".to_string(),
        }
    } else {
        config.server.host.to_string()
    };
    if host.contains(':') {
        format!("http://[{host}]:{}", config.server.port)
    } else {
        format!("http://{host}:{}", config.server.port)
    }
}

#[cfg(test)]
mod tests {
    use super::{complete, tool_catalog, McpEra};
    use serde_json::json;

    #[test]
    fn tool_catalog_is_read_execute_only() {
        let names = tool_catalog()
            .into_iter()
            .filter_map(|tool| tool.get("name").and_then(|value| value.as_str()).map(str::to_string))
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name == "llmgateway_resolve"));
        assert!(names.iter().any(|name| name == "llmgateway_chat"));
        for forbidden in ["delete", "disable", "enable", "reset", "restart", "credential"] {
            assert!(!names.iter().any(|name| name.contains(forbidden)));
        }
    }

    #[test]
    fn modern_results_are_discriminated() {
        assert_eq!(
            complete(json!({"tools":[]}), McpEra::Modern)["resultType"],
            "complete"
        );
        assert!(complete(json!({"tools":[]}), McpEra::Legacy)
            .get("resultType")
            .is_none());
    }
}
