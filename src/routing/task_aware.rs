use crate::config::{RouteConfig, RoutingConfig};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Coding,
    LongContext,
    Reasoning,
    SimpleChat,
    General,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskProfile {
    pub kind: TaskKind,
    pub estimated_input_tokens: usize,
    pub requested_output_tokens: usize,
    pub required_context_tokens: usize,
    pub coding: bool,
    pub reasoning: bool,
    pub long_context: bool,
    pub simple_chat: bool,
    pub explicit: bool,
    pub signals: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TaskFitSnapshot {
    pub adjustment: i32,
    pub matched_capabilities: Vec<String>,
    pub context_window: Option<i64>,
    pub context_sufficient: Option<bool>,
}

pub struct TaskRouteFit {
    pub snapshot: TaskFitSnapshot,
    pub exclusion_reason: Option<&'static str>,
}

pub fn classify(body: Option<&Value>, config: &RoutingConfig) -> TaskProfile {
    let Some(body) = body else {
        return general_profile();
    };

    let estimated_input_tokens = estimate_json_tokens(body);
    let requested_output_tokens = requested_output_tokens(body);
    let required_context_tokens = estimated_input_tokens.saturating_add(requested_output_tokens);
    let text = request_text(body).to_lowercase();
    let message_count = body
        .get("messages")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());

    let mut signals = Vec::new();
    let explicit_kind = body
        .get("llmgateway_task")
        .and_then(Value::as_str)
        .and_then(parse_task_kind);

    let long_context = required_context_tokens >= config.task_long_context_threshold_tokens;
    if long_context {
        signals.push("large_context".to_string());
    }

    let coding_score = coding_score(&text);
    let coding = coding_score >= 2;
    if coding {
        signals.push("coding_language_or_code".to_string());
    }

    let reasoning_score = reasoning_score(&text);
    let reasoning = reasoning_score >= 2;
    if reasoning {
        signals.push("reasoning_request".to_string());
    }

    let simple_chat = !long_context
        && !coding
        && !reasoning
        && !has_tools
        && estimated_input_tokens <= config.task_simple_max_input_tokens
        && message_count <= 4;
    if simple_chat {
        signals.push("small_simple_request".to_string());
    }

    let explicit = explicit_kind.is_some();
    let (kind, coding, reasoning, long_context, simple_chat) = match explicit_kind {
        Some(TaskKind::Coding) => (TaskKind::Coding, true, reasoning, long_context, false),
        Some(TaskKind::Reasoning) => (TaskKind::Reasoning, coding, true, long_context, false),
        Some(TaskKind::LongContext) => (TaskKind::LongContext, coding, reasoning, true, false),
        Some(TaskKind::SimpleChat) => (TaskKind::SimpleChat, false, false, long_context, true),
        Some(TaskKind::General) => (TaskKind::General, false, false, long_context, false),
        None if long_context => (TaskKind::LongContext, coding, reasoning, true, false),
        None if coding => (TaskKind::Coding, true, reasoning, false, false),
        None if reasoning => (TaskKind::Reasoning, false, true, false, false),
        None if simple_chat => (TaskKind::SimpleChat, false, false, false, true),
        None => (TaskKind::General, false, false, false, false),
    };

    if explicit {
        signals.push("explicit_task_hint".to_string());
    }

    TaskProfile {
        kind,
        estimated_input_tokens,
        requested_output_tokens,
        required_context_tokens,
        coding,
        reasoning,
        long_context,
        simple_chat,
        explicit,
        signals,
    }
}

pub fn route_fit(
    profile: &TaskProfile,
    route: &RouteConfig,
    config: &RoutingConfig,
) -> TaskRouteFit {
    let mut snapshot = TaskFitSnapshot {
        adjustment: 0,
        matched_capabilities: Vec::new(),
        context_window: route.context_window,
        context_sufficient: None,
    };

    if !config.task_aware_enabled {
        return TaskRouteFit {
            snapshot,
            exclusion_reason: None,
        };
    }

    if let Some(window) = route.context_window {
        let sufficient =
            window >= profile.required_context_tokens.min(i64::MAX as usize) as i64;
        snapshot.context_sufficient = Some(sufficient);
        if !sufficient {
            return TaskRouteFit {
                snapshot,
                exclusion_reason: Some("context_window_too_small"),
            };
        }
    }

    let capabilities = normalized_capabilities(&route.capabilities);
    let mut raw_adjustment = 0i32;
    let max_bonus = config.task_fit_max_bonus.max(0);
    let mismatch_penalty = config.task_mismatch_penalty.max(0);

    if profile.coding {
        if let Some(capability) =
            first_matching_capability(&capabilities, &["coding", "code", "developer"])
        {
            raw_adjustment = raw_adjustment.saturating_sub(max_bonus);
            push_unique(&mut snapshot.matched_capabilities, capability);
        }
    }

    if profile.reasoning {
        if let Some(capability) =
            first_matching_capability(&capabilities, &["reasoning", "deep-reasoning"])
        {
            raw_adjustment = raw_adjustment.saturating_sub(max_bonus);
            push_unique(&mut snapshot.matched_capabilities, capability);
        }
    }

    if profile.long_context {
        if let Some(capability) =
            first_matching_capability(&capabilities, &["long-context", "large-context"])
        {
            raw_adjustment = raw_adjustment.saturating_sub(max_bonus);
            push_unique(&mut snapshot.matched_capabilities, capability);
        } else if snapshot.context_sufficient == Some(true) {
            raw_adjustment = raw_adjustment.saturating_sub((max_bonus / 2).max(1));
        }
    }

    if profile.simple_chat {
        if let Some(capability) = first_matching_capability(
            &capabilities,
            &["cheap", "low-cost", "fast", "simple-chat"],
        ) {
            raw_adjustment = raw_adjustment.saturating_sub(max_bonus);
            push_unique(&mut snapshot.matched_capabilities, capability);
        }
        if first_matching_capability(&capabilities, &["premium", "expensive"]).is_some() {
            raw_adjustment =
                raw_adjustment.saturating_add((mismatch_penalty / 2).max(1));
        }
    }

    snapshot.adjustment = raw_adjustment.clamp(-max_bonus, mismatch_penalty);

    TaskRouteFit {
        snapshot,
        exclusion_reason: None,
    }
}

fn parse_task_kind(value: &str) -> Option<TaskKind> {
    match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "coding" | "code" => Some(TaskKind::Coding),
        "reasoning" | "reason" => Some(TaskKind::Reasoning),
        "long-context" | "longcontext" => Some(TaskKind::LongContext),
        "simple-chat" | "simple" | "cheap" => Some(TaskKind::SimpleChat),
        "general" | "auto" => Some(TaskKind::General),
        _ => None,
    }
}

fn coding_score(text: &str) -> usize {
    let mut score = 0usize;
    if text.contains("```") {
        score += 3;
    }
    for marker in [
        "debug",
        "compiler",
        "compile error",
        "stack trace",
        "exception",
        "refactor",
        "implement",
        "unit test",
        "integration test",
        "repository",
        "pull request",
        "java",
        "spring",
        "kafka",
        "python",
        "rust",
        "typescript",
        "javascript",
        "sql",
        "docker",
        "kubernetes",
        ".java",
        ".rs",
        ".py",
        "class ",
        "function ",
        "method ",
    ] {
        if text.contains(marker) {
            score += 1;
        }
    }
    score
}

fn reasoning_score(text: &str) -> usize {
    let mut score = 0usize;
    for marker in [
        "analyze",
        "analysis",
        "reason about",
        "explain why",
        "why did",
        "tradeoff",
        "trade-off",
        "compare",
        "evaluate",
        "root cause",
        "step by step",
        "derive",
        "prove",
        "architecture",
        "system design",
        "pros and cons",
    ] {
        if text.contains(marker) {
            score += 1;
        }
    }
    score
}

fn request_text(body: &Value) -> String {
    let mut parts = Vec::new();
    for key in ["messages", "input", "prompt"] {
        if let Some(value) = body.get(key) {
            collect_text(value, &mut parts);
        }
    }
    parts.join("\n")
}

fn collect_text(value: &Value, parts: &mut Vec<String>) {
    match value {
        Value::String(text) => parts.push(text.clone()),
        Value::Array(values) => {
            for value in values {
                collect_text(value, parts);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                parts.push(text.to_string());
            }
            if let Some(content) = map.get("content") {
                collect_text(content, parts);
            }
        }
        _ => {}
    }
}

fn requested_output_tokens(body: &Value) -> usize {
    body.get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(Value::as_u64)
        .map(|value| value.min(usize::MAX as u64) as usize)
        .unwrap_or(0)
}

fn estimate_json_tokens(value: &Value) -> usize {
    let bytes = serde_json::to_vec(value).map_or(0, |bytes| bytes.len());
    bytes.div_ceil(4)
}

fn normalized_capabilities(values: &[String]) -> HashSet<String> {
    values
        .iter()
        .map(|value| value.trim().to_ascii_lowercase().replace('_', "-"))
        .filter(|value| !value.is_empty())
        .collect()
}

fn first_matching_capability(
    capabilities: &HashSet<String>,
    expected: &[&str],
) -> Option<String> {
    expected
        .iter()
        .find(|candidate| capabilities.contains(**candidate))
        .map(|candidate| (*candidate).to_string())
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|item| item == &value) {
        values.push(value);
    }
}

fn general_profile() -> TaskProfile {
    TaskProfile {
        kind: TaskKind::General,
        estimated_input_tokens: 0,
        requested_output_tokens: 0,
        required_context_tokens: 0,
        coding: false,
        reasoning: false,
        long_context: false,
        simple_chat: false,
        explicit: false,
        signals: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoutingConfig;
    use serde_json::json;

    fn config() -> RoutingConfig {
        RoutingConfig {
            adaptive_enabled: true,
            adaptive_min_samples: 3,
            adaptive_history_samples: 100,
            adaptive_stale_after_seconds: 3_600,
            adaptive_ewma_alpha: 0.25,
            adaptive_latency_target_ms: 1_200,
            adaptive_max_penalty: 30,
            adaptive_failure_weight: 0.70,
            task_aware_enabled: true,
            task_fit_max_bonus: 20,
            task_mismatch_penalty: 12,
            task_long_context_threshold_tokens: 12_000,
            task_simple_max_input_tokens: 800,
            execution_preference: "browser-first".into(),
            api_fallback: true,
            browser_fairness_enabled: true,
            browser_recovery_penalty: 8,
            browser_recovery_max_penalty: 40,
            browser_sticky_affinity: true,
        }
    }

    fn route(capabilities: &[&str], context_window: Option<i64>) -> RouteConfig {
        RouteConfig {
            id: "route".into(),
            account: "account".into(),
            model: "model".into(),
            priority: 10,
            enabled: true,
            capabilities: capabilities.iter().map(|value| (*value).to_string()).collect(),
            context_window,
        }
    }

    #[test]
    fn classifies_coding_without_an_extra_llm_call() {
        let body = json!({
            "messages": [{
                "role": "user",
                "content": "Debug this Rust compiler error and refactor the function: ```rust\nfn main() {}\n```"
            }]
        });
        let profile = classify(Some(&body), &config());
        assert_eq!(profile.kind, TaskKind::Coding);
        assert!(profile.coding);
    }

    #[test]
    fn simple_chat_prefers_explicitly_cheap_or_fast_routes() {
        let body = json!({"messages":[{"role":"user","content":"hello"}]});
        let profile = classify(Some(&body), &config());
        assert_eq!(profile.kind, TaskKind::SimpleChat);
        let fit = route_fit(&profile, &route(&["chat", "cheap"], None), &config());
        assert_eq!(fit.snapshot.adjustment, -20);
    }

    #[test]
    fn known_too_small_context_window_is_excluded() {
        let body = json!({
            "llmgateway_task": "long_context",
            "max_tokens": 10_000,
            "messages": [{"role":"user","content":"summarize this"}]
        });
        let profile = classify(Some(&body), &config());
        let fit = route_fit(&profile, &route(&["chat"], Some(4_096)), &config());
        assert_eq!(fit.exclusion_reason, Some("context_window_too_small"));
        assert_eq!(fit.snapshot.context_sufficient, Some(false));
    }

    #[test]
    fn explicit_simple_chat_cannot_bypass_context_window_safety() {
        let body = json!({
            "llmgateway_task": "simple_chat",
            "max_tokens": 10_000,
            "messages": [{"role":"user","content":"hello"}]
        });
        let profile = classify(Some(&body), &config());
        assert_eq!(profile.kind, TaskKind::SimpleChat);
        assert!(!profile.long_context);
        assert!(profile.required_context_tokens > 4_096);
        let fit = route_fit(&profile, &route(&["chat", "cheap"], Some(4_096)), &config());
        assert_eq!(fit.exclusion_reason, Some("context_window_too_small"));
    }

    #[test]
    fn unknown_capabilities_remain_neutral_for_backward_compatibility() {
        let body = json!({
            "llmgateway_task": "coding",
            "messages": [{"role":"user","content":"implement it"}]
        });
        let profile = classify(Some(&body), &config());
        let fit = route_fit(&profile, &route(&[], None), &config());
        assert_eq!(fit.snapshot.adjustment, 0);
        assert!(fit.exclusion_reason.is_none());
    }
}
