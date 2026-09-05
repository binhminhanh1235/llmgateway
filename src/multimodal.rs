use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

pub const MULTIMODAL_SCHEMA_VERSION: u32 = 1;

#[derive(
    Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    File,
    Audio,
}

impl Modality {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::File => "file",
            Self::Audio => "audio",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent {
    Text {
        text: String,
    },
    Image {
        artifact_id: String,
        mime_type: Option<String>,
    },
    File {
        artifact_id: String,
        mime_type: Option<String>,
    },
    Audio {
        artifact_id: String,
        mime_type: Option<String>,
    },
}

impl InputContent {
    pub const fn modality(&self) -> Modality {
        match self {
            Self::Text { .. } => Modality::Text,
            Self::Image { .. } => Modality::Image,
            Self::File { .. } => Modality::File,
            Self::Audio { .. } => Modality::Audio,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContent {
    Text {
        text: String,
    },
    Image {
        artifact_id: String,
        mime_type: Option<String>,
    },
    Audio {
        artifact_id: String,
        mime_type: Option<String>,
    },
    File {
        artifact_id: String,
        mime_type: Option<String>,
    },
    ToolCall {
        call: ToolCall,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MultimodalMessage {
    pub role: String,
    pub content: Vec<InputContent>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MultimodalRequest {
    pub model: String,
    pub messages: Vec<MultimodalMessage>,
    pub output_modalities: Vec<Modality>,
    pub stream: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MultimodalUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MultimodalResponse {
    pub model: String,
    pub content: Vec<OutputContent>,
    pub usage: Option<MultimodalUsage>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub input_modalities: Vec<Modality>,
    pub output_modalities: Vec<Modality>,
    pub streaming: bool,
    pub native_file_upload: bool,
    pub image_generation: bool,
    pub image_editing: bool,
    pub audio_transcription: bool,
    pub supported_mime_types: Vec<String>,
    pub max_attachment_count: Option<u32>,
    pub max_attachment_size_bytes: Option<u64>,
}

impl ModelCapabilities {
    pub fn foundation_text_execution() -> Self {
        Self {
            input_modalities: vec![Modality::Text],
            output_modalities: vec![Modality::Text],
            streaming: true,
            ..Self::default()
        }
    }

    pub fn from_legacy_tags(tags: &[String]) -> Self {
        let normalized = tags
            .iter()
            .map(|tag| tag.trim().to_ascii_lowercase().replace('-', "_"))
            .collect::<BTreeSet<_>>();
        let has = |names: &[&str]| names.iter().any(|name| normalized.contains(*name));

        let mut input_modalities = vec![Modality::Text];
        if has(&["vision", "image", "image_input"]) {
            input_modalities.push(Modality::Image);
        }
        if has(&["file", "files", "file_input", "document", "documents"]) {
            input_modalities.push(Modality::File);
        }
        if has(&["audio", "audio_input", "transcription", "audio_transcription"]) {
            input_modalities.push(Modality::Audio);
        }

        let mut output_modalities = vec![Modality::Text];
        if has(&["image_generation", "image_output"]) {
            output_modalities.push(Modality::Image);
        }
        if has(&["audio_output", "speech", "text_to_speech"]) {
            output_modalities.push(Modality::Audio);
        }
        if has(&["file_output"]) {
            output_modalities.push(Modality::File);
        }

        Self {
            input_modalities,
            output_modalities,
            streaming: has(&["streaming", "stream"]),
            native_file_upload: has(&["native_file_upload", "file_upload"]),
            image_generation: has(&["image_generation"]),
            image_editing: has(&["image_editing", "image_edit"]),
            audio_transcription: has(&["audio_transcription", "transcription"]),
            supported_mime_types: Vec::new(),
            max_attachment_count: None,
            max_attachment_size_bytes: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AdapterCapabilities {
    pub id: String,
    pub transport: String,
    pub models: Vec<String>,
    pub capabilities: ModelCapabilities,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MultimodalError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("unsupported capability '{0}'")]
    UnsupportedCapability(String),
    #[error("unsupported input modality '{0}'")]
    UnsupportedInputModality(Modality),
    #[error("unsupported output modality '{0}'")]
    UnsupportedOutputModality(Modality),
}

impl MultimodalError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request_error",
            Self::UnsupportedCapability(_) => "unsupported_capability",
            Self::UnsupportedInputModality(_) => "unsupported_input_modality",
            Self::UnsupportedOutputModality(_) => "unsupported_output_modality",
        }
    }
}

impl std::fmt::Display for Modality {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub fn canonical_input_modalities() -> Vec<Modality> {
    vec![
        Modality::Text,
        Modality::Image,
        Modality::File,
        Modality::Audio,
    ]
}

pub fn canonical_output_modalities() -> Vec<Modality> {
    vec![
        Modality::Text,
        Modality::Image,
        Modality::Audio,
        Modality::File,
    ]
}

pub fn validate_foundation_execution(
    request: &MultimodalRequest,
) -> Result<(), MultimodalError> {
    validate_execution_modalities(request, &[Modality::Text])
}

pub fn validate_vision_execution(
    request: &MultimodalRequest,
) -> Result<(), MultimodalError> {
    validate_execution_modalities(request, &[Modality::Text, Modality::Image])
}

fn validate_execution_modalities(
    request: &MultimodalRequest,
    allowed_inputs: &[Modality],
) -> Result<(), MultimodalError> {
    for message in &request.messages {
        for content in &message.content {
            let modality = content.modality();
            if !allowed_inputs.contains(&modality) {
                return Err(MultimodalError::UnsupportedInputModality(modality));
            }
        }
    }
    for modality in &request.output_modalities {
        if *modality != Modality::Text {
            return Err(MultimodalError::UnsupportedOutputModality(*modality));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn structured_capabilities_serialize_stably() {
        let capabilities = ModelCapabilities {
            input_modalities: vec![Modality::Text, Modality::Image],
            output_modalities: vec![Modality::Text],
            streaming: true,
            native_file_upload: false,
            image_generation: false,
            image_editing: false,
            audio_transcription: false,
            supported_mime_types: vec!["image/png".into()],
            max_attachment_count: Some(4),
            max_attachment_size_bytes: Some(8_388_608),
        };
        assert_eq!(
            serde_json::to_value(capabilities).unwrap(),
            json!({
                "input_modalities":["text","image"],
                "output_modalities":["text"],
                "streaming":true,
                "native_file_upload":false,
                "image_generation":false,
                "image_editing":false,
                "audio_transcription":false,
                "supported_mime_types":["image/png"],
                "max_attachment_count":4,
                "max_attachment_size_bytes":8388608
            })
        );
    }

    #[test]
    fn legacy_capability_tags_map_without_mutating_legacy_contract() {
        let legacy = vec![
            "chat".to_string(),
            "streaming".to_string(),
            "vision".to_string(),
            "image-generation".to_string(),
        ];
        let original = legacy.clone();
        let structured = ModelCapabilities::from_legacy_tags(&legacy);
        assert_eq!(legacy, original);
        assert_eq!(
            structured.input_modalities,
            vec![Modality::Text, Modality::Image]
        );
        assert_eq!(
            structured.output_modalities,
            vec![Modality::Text, Modality::Image]
        );
        assert!(structured.streaming);
        assert!(structured.image_generation);
    }

    #[test]
    fn unsupported_modality_errors_have_deterministic_codes() {
        let request = MultimodalRequest {
            model: "test".into(),
            messages: vec![MultimodalMessage {
                role: "user".into(),
                content: vec![InputContent::Image {
                    artifact_id: "file_test".into(),
                    mime_type: Some("image/png".into()),
                }],
                tool_calls: vec![],
                tool_call_id: None,
                name: None,
            }],
            output_modalities: vec![Modality::Text],
            stream: false,
        };
        let error = validate_foundation_execution(&request).unwrap_err();
        assert_eq!(error.code(), "unsupported_input_modality");
        assert_eq!(error.to_string(), "unsupported input modality 'image'");
        assert!(validate_vision_execution(&request).is_ok());
    }
}
