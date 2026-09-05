use crate::{
    compat::{anthropic, responses},
    multimodal::{
        validate_foundation_execution, InputContent, Modality, MultimodalError,
        MultimodalMessage, MultimodalRequest, ToolCall,
    },
};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct NormalizedTextRequest {
    pub canonical: MultimodalRequest,
    execution_template: Value,
}

impl NormalizedTextRequest {
    pub fn into_current_execution(self) -> Value {
        self.execution_template
    }
}

pub fn normalize_chat_request(
    body: &Value,
    requested_model: &str,
) -> Result<NormalizedTextRequest, MultimodalError> {
    reject_requested_output_modalities(body)?;
    normalize_current_execution(body.clone(), requested_model.to_string())
}

pub fn normalize_responses_request(
    body: &Value,
) -> Result<NormalizedTextRequest, MultimodalError> {
    reject_responses_non_text_input(body)?;
    reject_requested_output_modalities(body)?;
    let (requested_model, execution) =
        responses::to_openai_request(body).map_err(MultimodalError::InvalidRequest)?;
    normalize_current_execution(execution, requested_model)
}

pub fn normalize_anthropic_request(
    body: &Value,
) -> Result<NormalizedTextRequest, MultimodalError> {
    reject_anthropic_non_text_input(body)?;
    let (requested_model, execution) =
        anthropic::to_openai_request(body).map_err(MultimodalError::InvalidRequest)?;
    normalize_current_execution(execution, requested_model)
}

fn normalize_current_execution(
    execution_template: Value,
    requested_model: String,
) -> Result<NormalizedTextRequest, MultimodalError> {
    let canonical = canonical_from_current_execution(&execution_template, requested_model)?;
    validate_foundation_execution(&canonical)?;
    Ok(NormalizedTextRequest {
        canonical,
        execution_template,
    })
}

fn canonical_from_current_execution(
    body: &Value,
    requested_model: String,
) -> Result<MultimodalRequest, MultimodalError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .map(parse_message)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();

    let output_modalities = requested_output_modalities(body)?;
    Ok(MultimodalRequest {
        model: requested_model,
        messages,
        output_modalities,
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn parse_message(message: &Value) -> Result<MultimodalMessage, MultimodalError> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user")
        .to_string();
    let content = parse_content(message.get("content").unwrap_or(&Value::Null))?;
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| calls.iter().filter_map(parse_tool_call).collect())
        .unwrap_or_default();

    Ok(MultimodalMessage {
        role,
        content,
        tool_calls,
        tool_call_id: message
            .get("tool_call_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        name: message
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn parse_tool_call(call: &Value) -> Option<ToolCall> {
    let function = call.get("function")?;
    Some(ToolCall {
        id: call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name: function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        arguments: function
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}")
            .to_string(),
    })
}

fn parse_content(content: &Value) -> Result<Vec<InputContent>, MultimodalError> {
    match content {
        Value::Null => Ok(Vec::new()),
        Value::String(text) => Ok(vec![InputContent::Text { text: text.clone() }]),
        Value::Array(items) => {
            let mut normalized = Vec::new();
            for item in items {
                match item {
                    Value::String(text) => {
                        normalized.push(InputContent::Text { text: text.clone() });
                    }
                    Value::Object(object) => {
                        let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
                        match kind {
                            "text" | "input_text" | "output_text" => {
                                let text = object
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                normalized.push(InputContent::Text { text });
                            }
                            "image" | "image_url" | "input_image" => {
                                return Err(MultimodalError::UnsupportedInputModality(
                                    Modality::Image,
                                ));
                            }
                            "file" | "input_file" | "document" => {
                                return Err(MultimodalError::UnsupportedInputModality(
                                    Modality::File,
                                ));
                            }
                            "audio" | "input_audio" => {
                                return Err(MultimodalError::UnsupportedInputModality(
                                    Modality::Audio,
                                ));
                            }
                            _ => {
                                if let Some(text) = object.get("text").and_then(Value::as_str) {
                                    normalized.push(InputContent::Text {
                                        text: text.to_string(),
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(normalized)
        }
        _ => Err(MultimodalError::InvalidRequest(
            "message content must be text, null, or a content array".into(),
        )),
    }
}

fn requested_output_modalities(body: &Value) -> Result<Vec<Modality>, MultimodalError> {
    let value = body
        .get("output_modalities")
        .or_else(|| body.get("modalities"));
    let Some(items) = value.and_then(Value::as_array) else {
        return Ok(vec![Modality::Text]);
    };
    let mut modalities = Vec::new();
    for item in items {
        let Some(name) = item.as_str() else {
            return Err(MultimodalError::InvalidRequest(
                "output modalities must be strings".into(),
            ));
        };
        let modality = modality_from_name(name).ok_or_else(|| {
            MultimodalError::UnsupportedCapability(format!("output_modality:{name}"))
        })?;
        if !modalities.contains(&modality) {
            modalities.push(modality);
        }
    }
    if modalities.is_empty() {
        modalities.push(Modality::Text);
    }
    Ok(modalities)
}

fn reject_requested_output_modalities(body: &Value) -> Result<(), MultimodalError> {
    for modality in requested_output_modalities(body)? {
        if modality != Modality::Text {
            return Err(MultimodalError::UnsupportedOutputModality(modality));
        }
    }
    Ok(())
}

fn modality_from_name(name: &str) -> Option<Modality> {
    match name.trim().to_ascii_lowercase().as_str() {
        "text" => Some(Modality::Text),
        "image" => Some(Modality::Image),
        "file" => Some(Modality::File),
        "audio" => Some(Modality::Audio),
        _ => None,
    }
}

fn reject_responses_non_text_input(body: &Value) -> Result<(), MultimodalError> {
    let Some(input) = body.get("input") else {
        return Ok(());
    };
    scan_responses_input(input)
}

fn scan_responses_input(value: &Value) -> Result<(), MultimodalError> {
    match value {
        Value::Array(items) => {
            for item in items {
                scan_responses_input(item)?;
            }
        }
        Value::Object(object) => {
            if let Some(kind) = object.get("type").and_then(Value::as_str) {
                match kind {
                    "input_image" | "image" | "image_url" => {
                        return Err(MultimodalError::UnsupportedInputModality(
                            Modality::Image,
                        ));
                    }
                    "input_file" | "file" | "document" => {
                        return Err(MultimodalError::UnsupportedInputModality(
                            Modality::File,
                        ));
                    }
                    "input_audio" | "audio" => {
                        return Err(MultimodalError::UnsupportedInputModality(
                            Modality::Audio,
                        ));
                    }
                    _ => {}
                }
            }
            if let Some(content) = object.get("content") {
                scan_responses_input(content)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn reject_anthropic_non_text_input(body: &Value) -> Result<(), MultimodalError> {
    if let Some(system) = body.get("system") {
        scan_anthropic_content(system)?;
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(content) = message.get("content") {
                scan_anthropic_content(content)?;
            }
        }
    }
    Ok(())
}

fn scan_anthropic_content(value: &Value) -> Result<(), MultimodalError> {
    let Some(items) = value.as_array() else {
        return Ok(());
    };
    for item in items {
        let Some(kind) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "image" => {
                return Err(MultimodalError::UnsupportedInputModality(
                    Modality::Image,
                ));
            }
            "document" | "file" => {
                return Err(MultimodalError::UnsupportedInputModality(
                    Modality::File,
                ));
            }
            "audio" => {
                return Err(MultimodalError::UnsupportedInputModality(
                    Modality::Audio,
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_signature(request: &MultimodalRequest) -> Vec<(String, String)> {
        request
            .messages
            .iter()
            .map(|message| {
                let text = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        InputContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                (message.role.clone(), text)
            })
            .collect()
    }

    #[test]
    fn responses_text_normalizes_to_canonical_request() {
        let normalized = normalize_responses_request(&json!({
            "model":"llmgateway-auto",
            "instructions":"be concise",
            "input":"hello",
            "stream":true
        }))
        .unwrap();
        assert_eq!(normalized.canonical.model, "llmgateway-auto");
        assert_eq!(
            text_signature(&normalized.canonical),
            vec![
                ("system".into(), "be concise".into()),
                ("user".into(), "hello".into())
            ]
        );
        assert!(normalized.canonical.stream);
    }

    #[test]
    fn chat_and_responses_text_are_semantically_equivalent() {
        let chat = normalize_chat_request(
            &json!({
                "model":"llmgateway-auto",
                "messages":[{"role":"user","content":"hello"}]
            }),
            "llmgateway-auto",
        )
        .unwrap();
        let responses = normalize_responses_request(&json!({
            "model":"llmgateway-auto",
            "input":"hello"
        }))
        .unwrap();
        assert_eq!(text_signature(&chat.canonical), text_signature(&responses.canonical));
        assert_eq!(chat.canonical.output_modalities, responses.canonical.output_modalities);
    }

    #[test]
    fn anthropic_text_uses_same_canonical_boundary() {
        let anthropic = normalize_anthropic_request(&json!({
            "model":"llmgateway-auto",
            "system":"be concise",
            "max_tokens":128,
            "messages":[{"role":"user","content":"hello"}]
        }))
        .unwrap();
        assert_eq!(
            text_signature(&anthropic.canonical),
            vec![
                ("system".into(), "be concise".into()),
                ("user".into(), "hello".into())
            ]
        );
    }

    #[test]
    fn canonical_bridge_preserves_current_execution_semantics() {
        let source = json!({
            "model":"llmgateway-auto",
            "messages":[{"role":"user","content":"hello"}],
            "temperature":0.2,
            "stream":false,
            "tools":[{
                "type":"function",
                "function":{"name":"lookup","parameters":{"type":"object"}}
            }]
        });
        let normalized = normalize_chat_request(&source, "llmgateway-auto").unwrap();
        let execution = normalized.into_current_execution();
        assert_eq!(execution, source);
    }

    #[test]
    fn known_multimodal_inputs_fail_with_capability_errors() {
        let chat = normalize_chat_request(
            &json!({
                "model":"llmgateway-auto",
                "messages":[{
                    "role":"user",
                    "content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]
                }]
            }),
            "llmgateway-auto",
        )
        .unwrap_err();
        assert_eq!(chat.code(), "unsupported_input_modality");

        let responses = normalize_responses_request(&json!({
            "model":"llmgateway-auto",
            "input":[{
                "role":"user",
                "content":[{"type":"input_file","file_id":"file_123"}]
            }]
        }))
        .unwrap_err();
        assert_eq!(responses.code(), "unsupported_input_modality");

        let output = normalize_chat_request(
            &json!({
                "model":"llmgateway-auto",
                "messages":[{"role":"user","content":"draw"}],
                "modalities":["image"]
            }),
            "llmgateway-auto",
        )
        .unwrap_err();
        assert_eq!(output.code(), "unsupported_output_modality");
    }
}
