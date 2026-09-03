use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use std::{convert::Infallible, pin::pin};
use tokio::sync::oneshot;
use uuid::Uuid;

pub fn responses_stream_with_capture<S>(
    upstream: S,
    completion: oneshot::Sender<Value>,
) -> impl Stream<Item = Result<Bytes, Infallible>>
where
    S: Stream<Item = Result<Bytes, Infallible>> + Send + 'static,
{
    async_stream::stream! {
        let mut upstream = pin!(upstream);
        let mut buffer = String::new();
        let mut completed: Option<Value> = None;

        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n"));
                    inspect_frames(&mut buffer, &mut completed);
                    yield Ok(bytes);
                }
                Err(error) => yield Err(error),
            }
        }
        inspect_frames(&mut buffer, &mut completed);
        if let Some(response) = completed {
            let _ = completion.send(response);
        }
    }
}

pub fn response_to_openai_assistant(response: &Value) -> Option<Value> {
    let output = response.get("output")?.as_array()?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();

    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if let Some(value) = part.get("text").and_then(Value::as_str) {
                            text.push_str(value);
                        }
                    }
                }
            }
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .cloned()
                    .unwrap_or_else(|| Value::String(format!("call_{}", Uuid::new_v4())));
                tool_calls.push(json!({
                    "id":call_id,
                    "type":"function",
                    "function":{
                        "name":item.get("name").cloned().unwrap_or_else(|| Value::String("tool".into())),
                        "arguments":item.get("arguments").cloned().unwrap_or_else(|| Value::String("{}".into()))
                    }
                }));
            }
            _ => {}
        }
    }

    if text.is_empty() && tool_calls.is_empty() {
        return None;
    }
    let mut assistant = json!({
        "role":"assistant",
        "content": if text.is_empty() { Value::Null } else { Value::String(text) }
    });
    if !tool_calls.is_empty() {
        assistant
            .as_object_mut()
            .expect("assistant is an object")
            .insert("tool_calls".into(), Value::Array(tool_calls));
    }
    Some(assistant)
}

fn inspect_frames(buffer: &mut String, completed: &mut Option<Value>) {
    while let Some(pos) = buffer.find("\n\n") {
        let frame = buffer[..pos].to_string();
        buffer.drain(..pos + 2);
        let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) == Some("response.completed") {
            *completed = event.get("response").cloned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_responses_output_back_to_openai_assistant() {
        let response = json!({
            "output":[
                {"type":"message","content":[{"type":"output_text","text":"hello"}]},
                {"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{}"}
            ]
        });
        let assistant = response_to_openai_assistant(&response).unwrap();
        assert_eq!(assistant["content"], "hello");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "read_file");
    }
}
