use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

pub const MEMORY_SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredMemory {
    #[serde(default)]
    pub facts: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub user_preferences: Vec<String>,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub code_context: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub rolling_summary: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct StructuredMemorySnapshot {
    pub thread_id: String,
    pub through_ordinal: i64,
    pub schema_version: i64,
    pub memory: StructuredMemory,
    pub model: String,
    pub route_id: Option<String>,
    pub updated_at: String,
}

impl StructuredMemory {
    pub fn normalize(mut self) -> Self {
        self.facts = normalize_items(self.facts);
        self.decisions = normalize_items(self.decisions);
        self.constraints = normalize_items(self.constraints);
        self.user_preferences = normalize_items(self.user_preferences);
        self.entities = normalize_items(self.entities);
        self.code_context = normalize_items(self.code_context);
        self.open_questions = normalize_items(self.open_questions);
        self.rolling_summary = self.rolling_summary.trim().to_string();
        self
    }

    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
            && self.decisions.is_empty()
            && self.constraints.is_empty()
            && self.user_preferences.is_empty()
            && self.entities.is_empty()
            && self.code_context.is_empty()
            && self.open_questions.is_empty()
            && self.rolling_summary.trim().is_empty()
    }

    pub fn render_for_prompt(&self) -> String {
        let mut out = String::from("Structured conversation memory (schema v1)\n");
        render_section(&mut out, "Facts", &self.facts);
        render_section(&mut out, "Decisions", &self.decisions);
        render_section(&mut out, "Constraints", &self.constraints);
        render_section(&mut out, "User preferences", &self.user_preferences);
        render_section(&mut out, "Entities", &self.entities);
        render_section(&mut out, "Code context", &self.code_context);
        render_section(&mut out, "Open questions", &self.open_questions);
        if !self.rolling_summary.is_empty() {
            out.push_str("\n## Rolling summary\n");
            out.push_str(&self.rolling_summary);
            out.push('\n');
        }
        out
    }

    pub fn from_legacy_text(text: impl Into<String>) -> Self {
        Self {
            rolling_summary: text.into().trim().to_string(),
            ..Self::default()
        }
    }
}

pub fn parse_model_memory(text: &str) -> Result<StructuredMemory, String> {
    let candidate = extract_json_object(text)
        .ok_or_else(|| "structured memory response did not contain a JSON object".to_string())?;
    let value: Value = serde_json::from_str(candidate)
        .map_err(|error| format!("invalid structured memory JSON: {error}"))?;
    let memory: StructuredMemory = serde_json::from_value(value)
        .map_err(|error| format!("invalid structured memory schema: {error}"))?;
    Ok(memory.normalize())
}

fn extract_json_object(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (start < end).then_some(&trimmed[start..=end])
}

fn normalize_items(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for item in items {
        let item = item.trim().trim_start_matches(['-', '*', '•']).trim().to_string();
        if item.is_empty() {
            continue;
        }
        let key = item.to_lowercase();
        if seen.insert(key) {
            result.push(item);
        }
    }
    result
}

fn render_section(out: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str("\n## ");
    out.push_str(title);
    out.push('\n');
    for item in items {
        out.push_str("- ");
        out.push_str(item);
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_model_memory, StructuredMemory};

    #[test]
    fn parses_fenced_structured_memory_and_deduplicates() {
        let memory = parse_model_memory(
            r#"```json
            {
              "facts":["Uses Rust", " uses rust "],
              "decisions":["Keep SQLite"],
              "constraints":[],
              "user_preferences":["Local-first"],
              "entities":["llmgateway"],
              "code_context":["ContextEngine owns compaction"],
              "open_questions":["Add browser sessions?"],
              "rolling_summary":"Building a local LLM gateway."
            }
            ```"#,
        )
        .expect("valid memory");
        assert_eq!(memory.facts, vec!["Uses Rust"]);
        assert_eq!(memory.decisions, vec!["Keep SQLite"]);
        assert_eq!(memory.rolling_summary, "Building a local LLM gateway.");
    }

    #[test]
    fn render_is_stable_and_sectioned() {
        let memory = StructuredMemory {
            facts: vec!["A".into()],
            decisions: vec!["B".into()],
            rolling_summary: "C".into(),
            ..StructuredMemory::default()
        };
        let rendered = memory.render_for_prompt();
        assert!(rendered.contains("## Facts\n- A"));
        assert!(rendered.contains("## Decisions\n- B"));
        assert!(rendered.contains("## Rolling summary\nC"));
    }
}
