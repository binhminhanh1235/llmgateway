use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::{collections::BTreeMap, convert::Infallible};
use uuid::Uuid;

pub fn to_openai_request(body: &Value) -> Result<(String, Value), String> {
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or("Responses request is missing model")?
        .to_string();

    if body
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
    {
        return Err(
            "previous_response_id is not supported yet; llmgateway v0.1 expects stateless Responses requests"
                .into(),
        );
    }

    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        if !instructions.is_empty() {
            messages.push(json!({"role":"system","content":instructions}));
        }
    }

    match body.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role":"user","content":text})),
        Some(Value::Array(items)) => {
            for item in items {
                translate_input_item(item, &mut messages)?;
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err("Responses input must be a string or an array".into()),
    }

    let mut out = Map::new();
    out.insert("model".into(), Value::String(requested_model.clone()));
    out.insert("messages".into(), Value::Array(messages));

    copy_if_present(body, &mut out, "temperature", "temperature");
    copy_if_present(body, &mut out, "top_p", "top_p");
    copy_if_present(body, &mut out, "stream", "stream");
    copy_if_present(
        body,
        &mut out,
        "parallel_tool_calls",
        "parallel_tool_calls",
    );
    copy_if_present(body, &mut out, "max_output_tokens", "max_tokens");

    if let Some(effort) = body
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
    {
        out.insert("reasoning_effort".into(), Value::String(effort.to_string()));
    }

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let translated = tools.iter().filter_map(translate_tool).collect::<Vec<_>>();
        if !translated.is_empty() {
            out.insert("tools".into(), Value::Array(translated));
        }
    }

    if let Some(choice) = body.get("tool_choice") {
        if let Some(mapped) = translate_tool_choice(choice) {
            out.insert("tool_choice".into(), mapped);
        }
    }

    Ok((requested_model, Value::Object(out)))
}

pub fn from_openai_response(openai: &Value, requested_model: &str) -> Value {
    let response_id = format!("resp_{}", Uuid::new_v4());
    let choice = openai
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    let message = choice.and_then(|choice| choice.get("message"));
    let mut output = Vec::new();

    if let Some(text) = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
    {
        if !text.is_empty() {
            output.push(json!({
                "id":format!("msg_{}", Uuid::new_v4()),
                "type":"message",
                "status":"completed",
                "role":"assistant",
                "content":[{"type":"output_text","text":text,"annotations":[]}]
            }));
        }
    }

    if let Some(tool_calls) = message
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for call in tool_calls {
            let function = call.get("function").unwrap_or(&Value::Null);
            output.push(json!({
                "id":format!("fc_{}", Uuid::new_v4()),
                "type":"function_call",
                "status":"completed",
                "call_id":call.get("id").cloned().unwrap_or_else(|| Value::String(format!("call_{}", Uuid::new_v4()))),
                "name":function.get("name").cloned().unwrap_or_else(|| Value::String("tool".into())),
                "arguments":function.get("arguments").cloned().unwrap_or_else(|| Value::String("{}".into()))
            }));
        }
    }

    let usage = openai.get("usage").cloned().unwrap_or_else(|| json!({}));
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    json!({
        "id":response_id,
        "object":"response",
        "created_at":chrono::Utc::now().timestamp(),
        "status":"completed",
        "error":null,
        "incomplete_details":null,
        "instructions":null,
        "model":requested_model,
        "output":output,
        "parallel_tool_calls":true,
        "tool_choice":"auto",
        "tools":[],
        "usage":{
            "input_tokens":input_tokens,
            "output_tokens":output_tokens,
            "total_tokens":input_tokens + output_tokens
        }
    })
}

pub fn openai_stream_to_responses(
    response: reqwest::Response,
    requested_model: String,
) -> impl futures_util::Stream<Item = Result<Bytes, Infallible>> {
    async_stream::stream! {
        let response_id = format!("resp_{}", Uuid::new_v4());
        let created_at = chrono::Utc::now().timestamp();
        let mut sequence = 0u64;
        let mut upstream = response.bytes_stream();
        let mut buffer = String::new();
        let mut text: Option<TextState> = None;
        let mut tools: BTreeMap<usize, ToolState> = BTreeMap::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut saw_done = false;

        yield Ok(event("response.created", with_sequence(json!({
            "type":"response.created",
            "response":response_shell(&response_id, &requested_model, created_at, "in_progress", vec![], 0, 0)
        }), &mut sequence)));

        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n"));
                    while let Some(pos) = buffer.find("\n\n") {
                        let frame = buffer[..pos].to_string();
                        buffer.drain(..pos + 2);
                        let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else { continue; };
                        if data == "[DONE]" {
                            saw_done = true;
                            continue;
                        }
                        let Ok(chunk) = serde_json::from_str::<Value>(data) else { continue; };

                        if let Some((code, message)) = openai_stream_error_details(&chunk) {
                            yield Ok(event("response.failed", with_sequence(json!({
                                "type":"response.failed",
                                "response":{
                                    "id":response_id,
                                    "object":"response",
                                    "status":"failed",
                                    "error":{"code":code,"message":message}
                                }
                            }), &mut sequence)));
                            return;
                        }

                        if let Some(usage) = chunk.get("usage") {
                            input_tokens = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(input_tokens);
                            output_tokens = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(output_tokens);
                        }

                        let Some(choice) = chunk.get("choices").and_then(Value::as_array).and_then(|items| items.first()) else { continue; };
                        let delta = choice.get("delta").unwrap_or(&Value::Null);

                        if let Some(delta_text) = delta.get("content").and_then(Value::as_str) {
                            if !delta_text.is_empty() {
                                if text.is_none() {
                                    let state = TextState::new(0);
                                    let item_id = state.item_id.clone();
                                    text = Some(state);
                                    yield Ok(event("response.output_item.added", with_sequence(json!({
                                        "type":"response.output_item.added",
                                        "output_index":0,
                                        "item":{"id":item_id,"type":"message","status":"in_progress","role":"assistant","content":[]}
                                    }), &mut sequence)));
                                    yield Ok(event("response.content_part.added", with_sequence(json!({
                                        "type":"response.content_part.added",
                                        "item_id":item_id,
                                        "output_index":0,
                                        "content_index":0,
                                        "part":{"type":"output_text","text":"","annotations":[]}
                                    }), &mut sequence)));
                                }
                                let state = text.as_mut().expect("text state initialized");
                                state.text.push_str(delta_text);
                                let item_id = state.item_id.clone();
                                yield Ok(event("response.output_text.delta", with_sequence(json!({
                                    "type":"response.output_text.delta",
                                    "item_id":item_id,
                                    "output_index":0,
                                    "content_index":0,
                                    "delta":delta_text
                                }), &mut sequence)));
                            }
                        }

                        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                            for tool_call in tool_calls {
                                let upstream_index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                                let output_index = if text.is_some() { upstream_index + 1 } else { upstream_index };
                                let mut start_event: Option<Value> = None;
                                let mut args_event: Option<Value> = None;

                                {
                                    let state = tools.entry(upstream_index).or_insert_with(|| ToolState::new(output_index));
                                    if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                                        state.call_id = id.to_string();
                                    }
                                    if let Some(name) = tool_call.get("function").and_then(|f| f.get("name")).and_then(Value::as_str) {
                                        state.name.push_str(name);
                                    }
                                    let args = tool_call
                                        .get("function")
                                        .and_then(|f| f.get("arguments"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("");

                                    if !state.started && !state.name.is_empty() {
                                        if state.call_id.is_empty() {
                                            state.call_id = format!("call_{}", Uuid::new_v4());
                                        }
                                        state.started = true;
                                        start_event = Some(json!({
                                            "type":"response.output_item.added",
                                            "output_index":state.output_index,
                                            "item":{
                                                "id":state.item_id,
                                                "type":"function_call",
                                                "status":"in_progress",
                                                "call_id":state.call_id,
                                                "name":state.name,
                                                "arguments":""
                                            }
                                        }));
                                    }

                                    if !args.is_empty() {
                                        state.arguments.push_str(args);
                                        if state.started {
                                            args_event = Some(json!({
                                                "type":"response.function_call_arguments.delta",
                                                "item_id":state.item_id,
                                                "output_index":state.output_index,
                                                "delta":args
                                            }));
                                        }
                                    }
                                }

                                if let Some(value) = start_event {
                                    yield Ok(event("response.output_item.added", with_sequence(value, &mut sequence)));
                                }
                                if let Some(value) = args_event {
                                    yield Ok(event("response.function_call_arguments.delta", with_sequence(value, &mut sequence)));
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    yield Ok(event("response.failed", with_sequence(json!({
                        "type":"response.failed",
                        "response":{
                            "id":response_id,
                            "object":"response",
                            "status":"failed",
                            "error":{"code":"upstream_stream_error","message":error.to_string()}
                        }
                    }), &mut sequence)));
                    return;
                }
            }
        }

        if !saw_done {
            yield Ok(event("response.failed", with_sequence(json!({
                "type":"response.failed",
                "response":{
                    "id":response_id,
                    "object":"response",
                    "status":"failed",
                    "error":{
                        "code":"upstream_stream_incomplete",
                        "message":"upstream stream ended before terminal [DONE] frame"
                    }
                }
            }), &mut sequence)));
            return;
        }

        let mut completed_output = Vec::new();

        if let Some(state) = text {
            let completed_item = json!({
                "id":state.item_id,
                "type":"message",
                "status":"completed",
                "role":"assistant",
                "content":[{"type":"output_text","text":state.text,"annotations":[]}]
            });
            yield Ok(event("response.output_text.done", with_sequence(json!({
                "type":"response.output_text.done",
                "item_id":state.item_id,
                "output_index":state.output_index,
                "content_index":0,
                "text":state.text
            }), &mut sequence)));
            yield Ok(event("response.content_part.done", with_sequence(json!({
                "type":"response.content_part.done",
                "item_id":state.item_id,
                "output_index":state.output_index,
                "content_index":0,
                "part":{"type":"output_text","text":state.text,"annotations":[]}
            }), &mut sequence)));
            yield Ok(event("response.output_item.done", with_sequence(json!({
                "type":"response.output_item.done",
                "output_index":state.output_index,
                "item":completed_item
            }), &mut sequence)));
            completed_output.push(completed_item);
        }

        let completed_tools = tools
            .values()
            .filter(|state| state.started)
            .map(|state| {
                let completed_item = json!({
                    "id":state.item_id,
                    "type":"function_call",
                    "status":"completed",
                    "call_id":state.call_id,
                    "name":state.name,
                    "arguments":state.arguments
                });
                let arguments_done = json!({
                    "type":"response.function_call_arguments.done",
                    "item_id":state.item_id,
                    "output_index":state.output_index,
                    "arguments":state.arguments
                });
                let item_done = json!({
                    "type":"response.output_item.done",
                    "output_index":state.output_index,
                    "item":completed_item
                });
                (completed_item, arguments_done, item_done)
            })
            .collect::<Vec<_>>();
        for (completed_item, arguments_done, item_done) in completed_tools {
            yield Ok(event(
                "response.function_call_arguments.done",
                with_sequence(arguments_done, &mut sequence),
            ));
            yield Ok(event(
                "response.output_item.done",
                with_sequence(item_done, &mut sequence),
            ));
            completed_output.push(completed_item);
        }

        completed_output.sort_by_key(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call")
        });
        let completed = response_shell(
            &response_id,
            &requested_model,
            created_at,
            "completed",
            completed_output,
            input_tokens,
            output_tokens,
        );
        yield Ok(event("response.completed", with_sequence(json!({
            "type":"response.completed",
            "response":completed
        }), &mut sequence)));
    }
}

fn openai_stream_error_details(chunk: &Value) -> Option<(String, String)> {
    let error = chunk.get("error")?;
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("upstream_stream_error")
        .to_string();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("upstream stream failed")
        .to_string();
    Some((code, message))
}

fn translate_input_item(item: &Value, out: &mut Vec<Value>) -> Result<(), String> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            let role = if role == "developer" { "system" } else { role };
            let content = item.get("content").unwrap_or(&Value::Null);
            let translated = translate_message_content(content);
            out.push(json!({"role":role,"content":translated}));
            Ok(())
        }
        Some("function_call") | Some("custom_tool_call") => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .cloned()
                .unwrap_or_else(|| Value::String(format!("call_{}", Uuid::new_v4())));
            out.push(json!({
                "role":"assistant",
                "content":null,
                "tool_calls":[{
                    "id":call_id,
                    "type":"function",
                    "function":{
                        "name":item.get("name").cloned().unwrap_or_else(|| Value::String("tool".into())),
                        "arguments":item.get("arguments").cloned().unwrap_or_else(|| Value::String("{}".into()))
                    }
                }]
            }));
            Ok(())
        }
        Some("function_call_output") | Some("custom_tool_call_output") => {
            let call_id = item.get("call_id").cloned().unwrap_or(Value::Null);
            let output = item.get("output").map(output_to_text).unwrap_or_default();
            out.push(json!({"role":"tool","tool_call_id":call_id,"content":output}));
            Ok(())
        }
        Some("reasoning") => Ok(()),
        Some(other) => Err(format!("unsupported Responses input item type '{other}'")),
        None => Err("Responses input item is missing type".into()),
    }
}

fn translate_message_content(content: &Value) -> Value {
    if let Some(text) = content.as_str() {
        return Value::String(text.to_string());
    }
    let Some(items) = content.as_array() else {
        return Value::String(String::new());
    };

    let parts = items
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") => Some(json!({
                "type":"text",
                "text":part.get("text").and_then(Value::as_str).unwrap_or("")
            })),
            Some("input_image") => part
                .get("image_url")
                .and_then(Value::as_str)
                .map(|url| json!({"type":"image_url","image_url":{"url":url}})),
            _ => None,
        })
        .collect::<Vec<_>>();

    Value::Array(parts)
}

fn translate_tool(tool: &Value) -> Option<Value> {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    let mut function = Map::new();
    function.insert("name".into(), tool.get("name")?.clone());
    if let Some(description) = tool.get("description") {
        function.insert("description".into(), description.clone());
    }
    function.insert(
        "parameters".into(),
        tool.get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type":"object"})),
    );
    if let Some(strict) = tool.get("strict") {
        function.insert("strict".into(), strict.clone());
    }
    Some(json!({"type":"function","function":Value::Object(function)}))
}

fn translate_tool_choice(choice: &Value) -> Option<Value> {
    if let Some(choice) = choice.as_str() {
        return Some(Value::String(choice.to_string()));
    }
    match choice.get("type").and_then(Value::as_str) {
        Some("function") => Some(json!({
            "type":"function",
            "function":{"name":choice.get("name")?.clone()}
        })),
        _ => None,
    }
}

fn output_to_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
    }
    value.to_string()
}

fn response_shell(
    id: &str,
    model: &str,
    created_at: i64,
    status: &str,
    output: Vec<Value>,
    input_tokens: u64,
    output_tokens: u64,
) -> Value {
    json!({
        "id":id,
        "object":"response",
        "created_at":created_at,
        "status":status,
        "error":null,
        "incomplete_details":null,
        "instructions":null,
        "model":model,
        "output":output,
        "parallel_tool_calls":true,
        "tool_choice":"auto",
        "tools":[],
        "usage":{
            "input_tokens":input_tokens,
            "output_tokens":output_tokens,
            "total_tokens":input_tokens + output_tokens
        }
    })
}

#[derive(Debug)]
struct TextState {
    output_index: usize,
    item_id: String,
    text: String,
}

impl TextState {
    fn new(output_index: usize) -> Self {
        Self {
            output_index,
            item_id: format!("msg_{}", Uuid::new_v4()),
            text: String::new(),
        }
    }
}

#[derive(Debug)]
struct ToolState {
    output_index: usize,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    started: bool,
}

impl ToolState {
    fn new(output_index: usize) -> Self {
        Self {
            output_index,
            item_id: format!("fc_{}", Uuid::new_v4()),
            call_id: String::new(),
            name: String::new(),
            arguments: String::new(),
            started: false,
        }
    }
}

fn event(name: &str, data: Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {}\n\n", data))
}

fn with_sequence(mut value: Value, sequence: &mut u64) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("sequence_number".into(), Value::from(*sequence));
    }
    *sequence += 1;
    value
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
                "message": "browser idle stream timeout"
            }
        });
        assert_eq!(
            openai_stream_error_details(&chunk),
            Some((
                "upstream_stream_error".to_string(),
                "browser idle stream timeout".to_string()
            ))
        );
        assert_eq!(openai_stream_error_details(&json!({"choices":[]})), None);
    }

    #[test]
    fn translates_codex_style_function_call_output() {
        let request = json!({
            "model":"llmgateway-coding",
            "instructions":"You are a coding agent",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"List files"}]},
                {"type":"function_call","call_id":"call_1","name":"shell","arguments":"{\"cmd\":\"ls\"}"},
                {"type":"function_call_output","call_id":"call_1","output":"Cargo.toml"}
            ],
            "tools":[{"type":"function","name":"shell","description":"run shell","parameters":{"type":"object"}}]
        });

        let (_, translated) = to_openai_request(&request).unwrap();
        assert_eq!(
            translated["messages"][2]["tool_calls"][0]["function"]["name"],
            "shell"
        );
        assert_eq!(translated["messages"][3]["role"], "tool");
        assert_eq!(translated["tools"][0]["function"]["name"], "shell");
    }
}
