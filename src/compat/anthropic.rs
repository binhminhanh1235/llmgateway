use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Map, Value};
use std::{collections::BTreeMap, convert::Infallible, fmt::Display, time::Duration};
use tokio::time::Instant;
use uuid::Uuid;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnthropicProtocolContext {
    pub version: Option<String>,
    pub betas: Vec<String>,
}

impl AnthropicProtocolContext {
    pub fn from_headers(version: Option<&str>, beta_values: &[String]) -> Self {
        let version = version
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let mut betas = Vec::new();
        for value in beta_values {
            for beta in value.split(',') {
                let beta = beta.trim();
                if !beta.is_empty() && !betas.iter().any(|existing| existing == beta) {
                    betas.push(beta.to_string());
                }
            }
        }
        Self { version, betas }
    }

    #[cfg(test)]
    pub fn beta_enabled(&self, beta: &str) -> bool {
        self.betas.iter().any(|value| value == beta)
    }
}

#[cfg(test)]
pub fn to_openai_request(body: &Value) -> Result<(String, Value), String> {
    to_openai_request_with_protocol(body, None, &[])
}

pub fn to_openai_request_with_protocol(
    body: &Value,
    anthropic_version: Option<&str>,
    anthropic_beta_values: &[String],
) -> Result<(String, Value), String> {
    let _protocol =
        AnthropicProtocolContext::from_headers(anthropic_version, anthropic_beta_values);

    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or("Anthropic request is missing model")?
        .to_string();

    let mut messages = Vec::new();
    if let Some(system) = body.get("system") {
        let text = content_to_text(system);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    if let Some(input_messages) = body.get("messages").and_then(Value::as_array) {
        for (index, message) in input_messages.iter().enumerate() {
            let has_later_user = input_messages[index + 1..]
                .iter()
                .any(|later| later.get("role").and_then(Value::as_str) == Some("user"));
            translate_message(message, &mut messages, has_later_user)?;
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), Value::String(requested_model.clone()));
    out.insert("messages".into(), Value::Array(messages));

    copy_if_present(body, &mut out, "temperature", "temperature");
    copy_if_present(body, &mut out, "top_p", "top_p");
    copy_if_present(body, &mut out, "max_tokens", "max_tokens");
    copy_if_present(body, &mut out, "stream", "stream");
    if let Some(stop) = body.get("stop_sequences") {
        out.insert("stop".into(), stop.clone());
    }

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let translated = tools
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.clone();
                let parameters = tool
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object"}));
                let mut function = Map::new();
                function.insert("name".into(), name);
                if let Some(description) = tool.get("description") {
                    function.insert("description".into(), description.clone());
                }
                function.insert("parameters".into(), parameters);
                Some(json!({"type": "function", "function": Value::Object(function)}))
            })
            .collect::<Vec<_>>();
        if !translated.is_empty() {
            out.insert("tools".into(), Value::Array(translated));
        }
    }

    if let Some(choice) = body.get("tool_choice") {
        if let Some(mapped) = translate_tool_choice(choice) {
            out.insert("tool_choice".into(), mapped);
        }
        if choice
            .get("disable_parallel_tool_use")
            .and_then(Value::as_bool)
            == Some(true)
        {
            out.insert("parallel_tool_calls".into(), Value::Bool(false));
        }
    }

    // Anthropic-only advisory/request-extension fields such as thinking,
    // cache_control, metadata, output_config and unknown beta fields are
    // intentionally consumed/ignored at this boundary instead of forwarded
    // to providers that do not implement Anthropic protocol extensions.
    Ok((requested_model, Value::Object(out)))
}

pub fn from_openai_response(openai: &Value, requested_model: &str) -> Value {
    let choice = openai
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    let message = choice.and_then(|choice| choice.get("message"));
    let mut content = Vec::new();

    if let Some(text) = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
    {
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }

    if let Some(tool_calls) = message
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for call in tool_calls {
            let function = call.get("function").unwrap_or(&Value::Null);
            let raw_args = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input =
                serde_json::from_str(raw_args).unwrap_or_else(|_| json!({"_raw": raw_args}));
            content.push(json!({
                "type": "tool_use",
                "id": call.get("id").cloned().unwrap_or_else(|| Value::String(format!("toolu_{}", Uuid::new_v4()))),
                "name": function.get("name").cloned().unwrap_or_else(|| Value::String("tool".into())),
                "input": input
            }));
        }
    }

    let finish_reason = choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str);
    let stop_reason = match finish_reason {
        Some("tool_calls") => "tool_use",
        Some("length") => "max_tokens",
        _ => "end_turn",
    };

    let usage = openai.get("usage").cloned().unwrap_or_else(|| json!({}));
    json!({
        "id": format!("msg_{}", Uuid::new_v4()),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": requested_model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0)
        }
    })
}

pub const ANTHROPIC_STREAM_PING_INTERVAL: Duration = Duration::from_secs(20);

pub fn openai_stream_to_anthropic(
    response: reqwest::Response,
    requested_model: String,
    request_id: String,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    openai_stream_to_anthropic_inner(
        response.bytes_stream(),
        requested_model,
        request_id,
        ANTHROPIC_STREAM_PING_INTERVAL,
    )
}

fn openai_stream_to_anthropic_inner<S, E>(
    upstream: S,
    requested_model: String,
    request_id: String,
    ping_interval: Duration,
) -> impl Stream<Item = Result<Bytes, Infallible>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Display + Send + 'static,
{
    async_stream::stream! {
        let message_id = format!("msg_{}", Uuid::new_v4());
        let mut upstream = Box::pin(upstream);
        let ping_timer = tokio::time::sleep(ping_interval);
        tokio::pin!(ping_timer);

        yield Ok(event("message_start", json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": requested_model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        })));

        let mut buffer = String::new();
        let mut next_block_index = 0usize;
        let mut text_block: Option<usize> = None;
        let mut tools: BTreeMap<usize, ToolStreamState> = BTreeMap::new();
        let mut stop_reason = "end_turn".to_string();
        let mut output_tokens = 0u64;
        let mut saw_terminal = false;

        'upstream: loop {
            let item = tokio::select! {
                _ = &mut ping_timer => {
                    yield Ok(event("ping", json!({"type":"ping"})));
                    ping_timer.as_mut().reset(Instant::now() + ping_interval);
                    continue;
                }
                item = upstream.next() => item,
            };

            let Some(item) = item else {
                break;
            };
            ping_timer.as_mut().reset(Instant::now() + ping_interval);

            match item {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n"));
                    while let Some(pos) = buffer.find("\n\n") {
                        let frame = buffer[..pos].to_string();
                        buffer.drain(..pos + 2);
                        let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else { continue; };
                        if data == "[DONE]" {
                            saw_terminal = true;
                            break 'upstream;
                        }
                        let Ok(chunk) = serde_json::from_str::<Value>(data) else { continue; };

                        if let Some(message) = openai_stream_error_message(&chunk) {
                            yield Ok(anthropic_stream_error(&request_id, &message));
                            return;
                        }

                        if let Some(usage) = chunk.get("usage") {
                            output_tokens = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(output_tokens);
                        }

                        let Some(choice) = chunk.get("choices").and_then(Value::as_array).and_then(|items| items.first()) else { continue; };
                        let mut terminal_in_frame = false;
                        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                            stop_reason = match reason {
                                "tool_calls" => "tool_use".into(),
                                "length" => "max_tokens".into(),
                                _ => "end_turn".into(),
                            };
                            terminal_in_frame = true;
                        }

                        let delta = choice.get("delta").unwrap_or(&Value::Null);
                        if let Some(text) = delta.get("content").and_then(Value::as_str) {
                            if !text.is_empty() {
                                let block_index = match text_block {
                                    Some(index) => index,
                                    None => {
                                        let index = next_block_index;
                                        next_block_index += 1;
                                        text_block = Some(index);
                                        yield Ok(event("content_block_start", json!({
                                            "type":"content_block_start",
                                            "index": index,
                                            "content_block":{"type":"text","text":""}
                                        })));
                                        index
                                    }
                                };
                                yield Ok(event("content_block_delta", json!({
                                    "type":"content_block_delta",
                                    "index": block_index,
                                    "delta":{"type":"text_delta","text":text}
                                })));
                            }
                        }

                        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                            for tool_call in tool_calls {
                                let upstream_index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                                let mut start_event = None;
                                let mut delta_event = None;
                                {
                                    let state = tools.entry(upstream_index).or_insert_with(|| {
                                        let block_index = next_block_index;
                                        next_block_index += 1;
                                        ToolStreamState::new(block_index)
                                    });
                                    if let Some(id) = tool_call.get("id").and_then(Value::as_str) { state.id = id.to_string(); }
                                    if let Some(name) = tool_call.get("function").and_then(|f| f.get("name")).and_then(Value::as_str) { state.name.push_str(name); }
                                    if let Some(args) = tool_call.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str) { state.pending_args.push_str(args); }

                                    if !state.started && !state.name.is_empty() {
                                        if state.id.is_empty() { state.id = format!("toolu_{}", Uuid::new_v4()); }
                                        state.started = true;
                                        start_event = Some(json!({
                                            "type":"content_block_start",
                                            "index":state.block_index,
                                            "content_block":{"type":"tool_use","id":state.id,"name":state.name,"input":{}}
                                        }));
                                    }
                                    if state.started && !state.pending_args.is_empty() {
                                        let partial = std::mem::take(&mut state.pending_args);
                                        delta_event = Some(json!({
                                            "type":"content_block_delta",
                                            "index":state.block_index,
                                            "delta":{"type":"input_json_delta","partial_json":partial}
                                        }));
                                    }
                                }
                                if let Some(value) = start_event {
                                    yield Ok(event("content_block_start", value));
                                }
                                if let Some(value) = delta_event {
                                    yield Ok(event("content_block_delta", value));
                                }
                            }
                        }

                        if terminal_in_frame {
                            saw_terminal = true;
                            break 'upstream;
                        }
                    }
                }
                Err(error) => {
                    yield Ok(anthropic_stream_error(&request_id, &error.to_string()));
                    return;
                }
            }
        }

        if !saw_terminal {
            let message = if buffer.trim().is_empty() {
                "upstream stream ended before a terminal finish_reason or [DONE] frame"
            } else {
                "upstream stream ended with a malformed or truncated terminal frame"
            };
            yield Ok(anthropic_stream_error(&request_id, message));
            return;
        }

        if let Some(index) = text_block {
            yield Ok(event("content_block_stop", json!({"type":"content_block_stop","index":index})));
        }
        let tool_stops = tools
            .values()
            .filter(|state| state.started)
            .map(|state| json!({"type":"content_block_stop","index":state.block_index}))
            .collect::<Vec<_>>();
        for value in tool_stops {
            yield Ok(event("content_block_stop", value));
        }
        yield Ok(event("message_delta", json!({
            "type":"message_delta",
            "delta":{"stop_reason":stop_reason,"stop_sequence":null},
            "usage":{"output_tokens":output_tokens}
        })));
        yield Ok(event("message_stop", json!({"type":"message_stop"})));
    }
}

fn anthropic_stream_error(request_id: &str, message: &str) -> Bytes {
    event(
        "error",
        json!({
            "type":"error",
            "error":{"type":"api_error","message":message},
            "request_id":request_id
        }),
    )
}

fn openai_stream_error_message(chunk: &Value) -> Option<String> {
    let error = chunk.get("error")?;
    Some(
        error
            .get("message")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("upstream stream failed")
            .to_string(),
    )
}

#[derive(Debug)]
struct ToolStreamState {
    block_index: usize,
    id: String,
    name: String,
    pending_args: String,
    started: bool,
}

impl ToolStreamState {
    fn new(block_index: usize) -> Self {
        Self {
            block_index,
            id: String::new(),
            name: String::new(),
            pending_args: String::new(),
            started: false,
        }
    }
}

fn event(name: &str, data: Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {}\n\n", data))
}

fn translate_message(
    message: &Value,
    out: &mut Vec<Value>,
    has_later_user: bool,
) -> Result<(), String> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .ok_or("message is missing role")?;
    let content = message.get("content").unwrap_or(&Value::Null);

    if role == "system" {
        let cleared = message
            .get("clear_at")
            .and_then(Value::as_str)
            .is_some_and(|value| value == "next_user_message" && has_later_user);
        let text = if cleared {
            String::new()
        } else {
            content_to_text(content)
        };
        out.push(json!({"role":"system","content":text}));
        return Ok(());
    }

    if role == "assistant" {
        let blocks = content
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![json!({"type":"text","text":content.as_str().unwrap_or("")})]);
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    text.push_str(block.get("text").and_then(Value::as_str).unwrap_or(""))
                }
                Some("tool_use") => {
                    let args = serde_json::to_string(block.get("input").unwrap_or(&json!({})))
                        .unwrap_or_else(|_| "{}".into());
                    tool_calls.push(json!({
                        "id": block.get("id").cloned().unwrap_or_else(|| Value::String(format!("call_{}", Uuid::new_v4()))),
                        "type":"function",
                        "function":{
                            "name":block.get("name").cloned().unwrap_or_else(|| Value::String("tool".into())),
                            "arguments":args
                        }
                    }));
                }
                _ => {}
            }
        }
        let mut translated = Map::new();
        translated.insert("role".into(), Value::String("assistant".into()));
        if !text.is_empty() {
            translated.insert("content".into(), Value::String(text));
        }
        if !tool_calls.is_empty() {
            translated.insert("tool_calls".into(), Value::Array(tool_calls));
        }
        if !translated.contains_key("content") {
            translated.insert("content".into(), Value::Null);
        }
        out.push(Value::Object(translated));
        return Ok(());
    }

    if role == "user" {
        if let Some(blocks) = content.as_array() {
            let mut user_parts = Vec::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        if !user_parts.is_empty() {
                            out.push(
                                json!({"role":"user","content":std::mem::take(&mut user_parts)}),
                            );
                        }
                        out.push(json!({
                            "role":"tool",
                            "tool_call_id":block.get("tool_use_id").cloned().unwrap_or(Value::Null),
                            "content":content_to_text(block.get("content").unwrap_or(&Value::Null))
                        }));
                    }
                    Some("text") => user_parts.push(json!({
                        "type":"text",
                        "text":block.get("text").and_then(Value::as_str).unwrap_or("")
                    })),
                    Some("image") => {
                        if let Some(part) = translate_image(block) {
                            user_parts.push(part);
                        }
                    }
                    _ => {}
                }
            }
            if !user_parts.is_empty() {
                out.push(json!({"role":"user","content":user_parts}));
            }
        } else {
            out.push(json!({"role":"user","content":content_to_text(content)}));
        }
        return Ok(());
    }

    Err(format!("unsupported Anthropic message role '{role}'"))
}

fn translate_image(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media_type = source.get("media_type")?.as_str()?;
            let data = source.get("data")?.as_str()?;
            Some(json!({
                "type":"image_url",
                "image_url":{"url":format!("data:{media_type};base64,{data}")}
            }))
        }
        Some("url") => {
            let url = source.get("url")?.as_str()?;
            Some(json!({"type":"image_url","image_url":{"url":url}}))
        }
        _ => None,
    }
}

fn translate_tool_choice(choice: &Value) -> Option<Value> {
    match choice.get("type").and_then(Value::as_str) {
        Some("auto") => Some(Value::String("auto".into())),
        Some("any") => Some(Value::String("required".into())),
        Some("tool") => Some(json!({
            "type":"function",
            "function":{"name":choice.get("name")?.clone()}
        })),
        Some("none") => Some(Value::String("none".into())),
        _ => None,
    }
}

fn content_to_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(blocks) = value.as_array() {
        return blocks
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    block.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn copy_if_present(source: &Value, target: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = source.get(from) {
        target.insert(to.to_string(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_openai_style_stream_error_frames() {
        let chunk = json!({
            "error": {
                "code": "upstream_stream_error",
                "message": "browser stream poll failed"
            }
        });
        assert_eq!(
            openai_stream_error_message(&chunk),
            Some("browser stream poll failed".to_string())
        );
        assert_eq!(openai_stream_error_message(&json!({"choices":[]})), None);
    }

    #[test]
    fn translates_tool_use_and_tool_result() {
        let request = json!({
            "model":"llmgateway-coding",
            "max_tokens":1024,
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"read_file","input":{"path":"a.rs"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"hello"}]}
            ],
            "tools":[{"name":"read_file","description":"Read","input_schema":{"type":"object"}}]
        });
        let (_, openai) = to_openai_request(&request).unwrap();
        assert_eq!(
            openai["messages"][0]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(openai["messages"][1]["role"], "tool");
    }

    #[test]
    fn translates_top_level_system_before_user() {
        let request = json!({
            "model":"deepseek-web/deepseek-web-default",
            "system":"You are Claude Code.",
            "messages":[{"role":"user","content":"hello"}]
        });
        let (_, openai) = to_openai_request(&request).unwrap();
        assert_eq!(
            openai["messages"][0],
            json!({"role":"system","content":"You are Claude Code."})
        );
        assert_eq!(openai["messages"][1]["role"], "user");
    }

    #[test]
    fn preserves_mid_conversation_system_order() {
        let request = json!({
            "model":"gemini-web/gemini-web-flash",
            "messages":[
                {"role":"user","content":"first"},
                {"role":"system","content":"new operator instruction"},
                {"role":"user","content":"second"}
            ]
        });
        let (_, openai) = to_openai_request(&request).unwrap();
        let roles = openai["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(roles, vec!["user", "system", "user"]);
        assert_eq!(openai["messages"][1]["content"], "new operator instruction");
    }

    #[test]
    fn translates_system_content_block_array_and_ignores_cache_control() {
        let request = json!({
            "model":"llmgateway-coding",
            "messages":[{
                "role":"system",
                "content":[
                    {"type":"text","text":"first","cache_control":{"type":"ephemeral"}},
                    {"type":"text","text":"second"}
                ]
            }]
        });
        let (_, openai) = to_openai_request(&request).unwrap();
        assert_eq!(
            openai["messages"][0],
            json!({"role":"system","content":"first\nsecond"})
        );
    }

    #[test]
    fn clears_turn_scoped_system_after_later_user_without_reordering() {
        let request = json!({
            "model":"llmgateway-coding",
            "messages":[
                {"role":"user","content":"turn one"},
                {"role":"system","clear_at":"next_user_message","content":"only next turn"},
                {"role":"user","content":"turn two"}
            ]
        });
        let (_, openai) = to_openai_request(&request).unwrap();
        assert_eq!(openai["messages"][1], json!({"role":"system","content":""}));
        assert_eq!(openai["messages"][2]["content"], "turn two");
    }

    #[test]
    fn keeps_active_turn_scoped_system_when_no_later_user_exists() {
        let request = json!({
            "model":"llmgateway-coding",
            "messages":[
                {"role":"user","content":"turn one"},
                {"role":"system","clear_at":"next_user_message","content":"apply now"}
            ]
        });
        let (_, openai) = to_openai_request(&request).unwrap();
        assert_eq!(
            openai["messages"][1],
            json!({"role":"system","content":"apply now"})
        );
    }

    #[test]
    fn accepts_effort_only_system_and_optional_beta_fields() {
        let request = json!({
            "model":"llmgateway-coding",
            "thinking":{"type":"adaptive"},
            "output_config":{"effort":"high"},
            "cache_control":{"type":"ephemeral"},
            "metadata":{"user_id":"claude-code"},
            "future_beta":{"enabled":true},
            "messages":[
                {"role":"user","content":"plan"},
                {"role":"system","content":[],"output_config":{"effort":"low"}},
                {"role":"user","content":"summarize"}
            ]
        });
        let (_, openai) = to_openai_request(&request).unwrap();
        assert_eq!(openai["messages"][1], json!({"role":"system","content":""}));
        assert!(openai.get("thinking").is_none());
        assert!(openai.get("output_config").is_none());
        assert!(openai.get("future_beta").is_none());
    }

    #[test]
    fn system_tools_tool_result_and_tool_choice_normalize_together() {
        let request = json!({
            "model":"llmgateway-coding",
            "messages":[
                {"role":"system","content":"Use tools when needed."},
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"read_file","input":{"path":"a.rs"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"hello"}]}]}
            ],
            "tools":[{"name":"read_file","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"auto","disable_parallel_tool_use":true}
        });
        let (_, openai) = to_openai_request(&request).unwrap();
        assert_eq!(openai["messages"][0]["role"], "system");
        assert_eq!(
            openai["messages"][1]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(openai["messages"][2]["role"], "tool");
        assert_eq!(openai["parallel_tool_calls"], false);
    }

    #[test]
    fn streaming_system_request_is_accepted() {
        let request = json!({
            "model":"llmgateway-coding",
            "stream":true,
            "system":[{"type":"text","text":"stream safely"}],
            "messages":[{"role":"user","content":"hello"}]
        });
        let (_, openai) = to_openai_request(&request).unwrap();
        assert_eq!(openai["stream"], true);
        assert_eq!(openai["messages"][0]["role"], "system");
    }

    #[test]
    fn parses_anthropic_protocol_headers_without_rejecting_unknown_betas() {
        let betas = vec![
            "mid-conversation-output-config-2026-07-01, future-beta-2099-01-01".to_string(),
            "mid-conversation-system-clear-at-2026-08-21".to_string(),
        ];
        let protocol = AnthropicProtocolContext::from_headers(Some("2023-06-01"), &betas);
        assert_eq!(protocol.version.as_deref(), Some("2023-06-01"));
        assert!(protocol.beta_enabled("mid-conversation-output-config-2026-07-01"));
        assert!(protocol.beta_enabled("mid-conversation-system-clear-at-2026-08-21"));
        assert!(protocol.beta_enabled("future-beta-2099-01-01"));
    }

    #[test]
    fn accepts_claude_code_like_fixture() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../fixtures/claude-code-messages.json")).unwrap();
        let headers = fixture["headers"].as_object().unwrap();
        let version = headers.get("anthropic-version").and_then(Value::as_str);
        let beta_values = headers
            .get("anthropic-beta")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        let (_, openai) =
            to_openai_request_with_protocol(&fixture["body"], version, &beta_values).unwrap();
        assert_eq!(openai["stream"], true);
        assert_eq!(openai["tool_choice"], "auto");
        assert_eq!(openai["parallel_tool_calls"], false);
        let roles = openai["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            vec![
                "system",
                "user",
                "assistant",
                "tool",
                "system",
                "system",
                "user"
            ]
        );
        assert_eq!(openai["messages"][4]["content"], "");
        assert_eq!(openai["messages"][5]["content"], "");
    }

    #[tokio::test(start_paused = true)]
    async fn emits_anthropic_ping_during_silent_gap() {
        let upstream = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
        let stream = openai_stream_to_anthropic_inner(
            upstream,
            "model".into(),
            "req_ping".into(),
            Duration::from_secs(20),
        );
        tokio::pin!(stream);

        let start = stream.next().await.expect("message_start").unwrap();
        assert!(String::from_utf8_lossy(&start).contains("event: message_start"));

        tokio::time::advance(Duration::from_secs(20)).await;
        let ping = stream.next().await.expect("ping").unwrap();
        let ping = String::from_utf8_lossy(&ping);
        assert!(ping.contains("event: ping"));
        assert!(ping.contains(r#""type":"ping""#));
    }

    #[tokio::test]
    async fn terminal_finish_reason_does_not_require_done_frame() {
        let upstream =
            futures_util::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(concat!(
                r#"data: {"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}"#,
                "\n\n",
            )))]);
        let events = openai_stream_to_anthropic_inner(
            upstream,
            "model".into(),
            "req_terminal".into(),
            Duration::from_secs(20),
        )
        .collect::<Vec<_>>()
        .await;
        let rendered = events
            .into_iter()
            .map(|event| String::from_utf8_lossy(&event.unwrap()).into_owned())
            .collect::<String>();
        assert!(rendered.contains("event: message_stop"));
        assert!(!rendered.contains("event: error"));
    }

    #[tokio::test]
    async fn malformed_or_truncated_stream_errors_without_message_stop() {
        let upstream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(
            r#"data: {"choices":["#,
        ))]);
        let events = openai_stream_to_anthropic_inner(
            upstream,
            "model".into(),
            "req_truncated".into(),
            Duration::from_secs(20),
        )
        .collect::<Vec<_>>()
        .await;
        let rendered = events
            .into_iter()
            .map(|event| String::from_utf8_lossy(&event.unwrap()).into_owned())
            .collect::<String>();
        assert!(rendered.contains("event: error"));
        assert!(rendered.contains(r#""request_id":"req_truncated""#));
        assert!(!rendered.contains("event: message_stop"));
    }

    #[test]
    fn preserves_max_tokens_and_long_tool_history_over_twenty_turns() {
        let mut messages = Vec::new();
        messages.push(json!({"role":"user","content":"inspect the repository"}));
        for turn in 0..24 {
            let tool_id = format!("toolu_{turn}");
            messages.push(json!({
                "role":"assistant",
                "content":[{
                    "type":"tool_use",
                    "id":tool_id,
                    "name":"read_file",
                    "input":{"path":format!("src/file_{turn}.rs")}
                }]
            }));
            messages.push(json!({
                "role":"user",
                "content":[{
                    "type":"tool_result",
                    "tool_use_id":format!("toolu_{turn}"),
                    "content":format!("fixture result {turn}")
                }]
            }));
        }
        let request = json!({
            "model":"llmgateway-coding",
            "max_tokens":4096,
            "stream":true,
            "messages":messages,
            "tools":[{
                "name":"read_file",
                "description":"Read",
                "input_schema":{"type":"object","properties":{"path":{"type":"string"}}}
            }],
            "tool_choice":{"type":"auto","disable_parallel_tool_use":true}
        });
        let (_, openai) = to_openai_request(&request).unwrap();
        assert_eq!(openai["max_tokens"], 4096);
        assert_eq!(openai["stream"], true);
        assert_eq!(openai["tool_choice"], "auto");
        assert_eq!(openai["parallel_tool_calls"], false);
        assert!(
            openai["messages"].as_array().unwrap().len() >= 49,
            "20+ sequential Anthropic tool/model turns must remain representable"
        );
    }

    #[tokio::test]
    async fn upstream_error_frame_never_becomes_message_stop_success() {
        let upstream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
            Bytes::from(
                "data: {\"error\":{\"type\":\"upstream_stream_error\",\"message\":\"forced transient failure\"}}\n\n",
            ),
        )]);
        let events = openai_stream_to_anthropic_inner(
            upstream,
            "model".into(),
            "req_stream_error".into(),
            Duration::from_secs(20),
        )
        .collect::<Vec<_>>()
        .await;
        let rendered = events
            .into_iter()
            .map(|event| String::from_utf8_lossy(&event.unwrap()).into_owned())
            .collect::<String>();
        assert!(rendered.contains("event: error"));
        assert!(rendered.contains("forced transient failure"));
        assert!(!rendered.contains("event: message_stop"));
    }
}
