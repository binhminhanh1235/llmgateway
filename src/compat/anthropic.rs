use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::{collections::BTreeMap, convert::Infallible};
use uuid::Uuid;

pub fn to_openai_request(body: &Value) -> Result<(String, Value), String> {
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
        for message in input_messages {
            translate_message(message, &mut messages)?;
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
    }

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
            let input = serde_json::from_str(raw_args).unwrap_or_else(|_| json!({"_raw": raw_args}));
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

pub fn openai_stream_to_anthropic(
    response: reqwest::Response,
    requested_model: String,
) -> impl futures_util::Stream<Item = Result<Bytes, Infallible>> {
    async_stream::stream! {
        let message_id = format!("msg_{}", Uuid::new_v4());
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

        let mut upstream = response.bytes_stream();
        let mut buffer = String::new();
        let mut next_block_index = 0usize;
        let mut text_block: Option<usize> = None;
        let mut tools: BTreeMap<usize, ToolStreamState> = BTreeMap::new();
        let mut stop_reason = "end_turn".to_string();
        let mut output_tokens = 0u64;

        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n"));
                    while let Some(pos) = buffer.find("\n\n") {
                        let frame = buffer[..pos].to_string();
                        buffer.drain(..pos + 2);
                        let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else { continue; };
                        if data == "[DONE]" { continue; }
                        let Ok(chunk) = serde_json::from_str::<Value>(data) else { continue; };

                        if let Some(usage) = chunk.get("usage") {
                            output_tokens = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(output_tokens);
                        }

                        let Some(choice) = chunk.get("choices").and_then(Value::as_array).and_then(|items| items.first()) else { continue; };
                        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                            stop_reason = match reason {
                                "tool_calls" => "tool_use".into(),
                                "length" => "max_tokens".into(),
                                _ => "end_turn".into(),
                            };
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
                    }
                }
                Err(error) => {
                    yield Ok(event("error", json!({
                        "type":"error",
                        "error":{"type":"api_error","message":error.to_string()}
                    })));
                    return;
                }
            }
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

fn translate_message(message: &Value, out: &mut Vec<Value>) -> Result<(), String> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .ok_or("message is missing role")?;
    let content = message.get("content").unwrap_or(&Value::Null);

    if role == "assistant" {
        let blocks = content.as_array().cloned().unwrap_or_else(|| {
            vec![json!({"type":"text","text":content.as_str().unwrap_or("")})]
        });
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
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
                            out.push(json!({"role":"user","content":std::mem::take(&mut user_parts)}));
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
}
