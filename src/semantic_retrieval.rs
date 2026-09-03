use crate::conversation::StoredMessage;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Serialize)]
pub struct RetrievedChunk {
    pub start_ordinal: i64,
    pub end_ordinal: i64,
    pub score: f64,
    pub preview: String,
}

#[derive(Clone, Debug)]
pub struct RetrievalResult {
    pub chunks: Vec<RetrievedChunk>,
    pub rendered: String,
    pub estimated_tokens: usize,
}

#[derive(Clone, Debug)]
struct CandidateChunk {
    start_ordinal: i64,
    end_ordinal: i64,
    text: String,
    terms: Vec<String>,
}

pub fn retrieve_relevant_history(
    messages: &[StoredMessage],
    through_ordinal: i64,
    query_message: &Value,
    max_chunks: usize,
    max_tokens: usize,
    min_score: f64,
) -> RetrievalResult {
    if max_chunks == 0 || max_tokens == 0 || through_ordinal <= 0 {
        return RetrievalResult {
            chunks: Vec::new(),
            rendered: String::new(),
            estimated_tokens: 0,
        };
    }

    let query = message_text(query_message);
    let query_terms = tokenize(&query);
    if query_terms.is_empty() {
        return RetrievalResult {
            chunks: Vec::new(),
            rendered: String::new(),
            estimated_tokens: 0,
        };
    }

    let candidates = conversation_chunks(messages, through_ordinal);
    if candidates.is_empty() {
        return RetrievalResult {
            chunks: Vec::new(),
            rendered: String::new(),
            estimated_tokens: 0,
        };
    }

    let document_frequency = document_frequency(&candidates);
    let document_count = candidates.len() as f64;
    let max_ordinal = candidates
        .iter()
        .map(|chunk| chunk.end_ordinal)
        .max()
        .unwrap_or(through_ordinal)
        .max(1) as f64;
    let query_bigrams = bigrams(&query_terms);

    let mut scored = candidates
        .into_iter()
        .filter_map(|candidate| {
            let score = score_candidate(
                &candidate,
                &query_terms,
                &query_bigrams,
                &document_frequency,
                document_count,
                max_ordinal,
            );
            (score >= min_score).then_some((candidate, score))
        })
        .collect::<Vec<_>>();

    scored.sort_by(|(a, score_a), (b, score_b)| {
        score_b
            .partial_cmp(score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.end_ordinal.cmp(&a.end_ordinal))
    });

    let mut selected = Vec::new();
    let mut rendered_sections = Vec::new();
    let mut used_tokens = 0usize;

    for (candidate, score) in scored
        .into_iter()
        .take(max_chunks.saturating_mul(3).max(max_chunks))
    {
        let section = format!(
            "Earlier transcript ordinals {}-{}:\n{}",
            candidate.start_ordinal, candidate.end_ordinal, candidate.text
        );
        let section_tokens = estimate_text_tokens(&section);
        if !selected.is_empty() && used_tokens.saturating_add(section_tokens) > max_tokens {
            continue;
        }
        if selected.is_empty() && section_tokens > max_tokens {
            let truncated = truncate_to_token_budget(&section, max_tokens);
            used_tokens = estimate_text_tokens(&truncated);
            rendered_sections.push(truncated);
        } else {
            used_tokens = used_tokens.saturating_add(section_tokens);
            rendered_sections.push(section);
        }
        selected.push(RetrievedChunk {
            start_ordinal: candidate.start_ordinal,
            end_ordinal: candidate.end_ordinal,
            score: (score * 1000.0).round() / 1000.0,
            preview: preview(&candidate.text, 180),
        });
        if selected.len() >= max_chunks || used_tokens >= max_tokens {
            break;
        }
    }

    RetrievalResult {
        chunks: selected,
        rendered: rendered_sections.join("\n\n"),
        estimated_tokens: used_tokens,
    }
}

pub fn augment_messages_with_retrieval(messages: &mut Vec<Value>, retrieval: &RetrievalResult) {
    if retrieval.chunks.is_empty() || retrieval.rendered.trim().is_empty() {
        return;
    }
    let block = format!(
        "Retrieved earlier transcript excerpts that are specifically relevant to the current request. Use them as supporting historical evidence. Durable memory and recent verbatim messages remain higher priority if there is a conflict.\n\n{}",
        retrieval.rendered
    );

    if let Some(first) = messages.first_mut() {
        if first.get("role").and_then(Value::as_str) == Some("system") {
            if let Some(content) = first.get_mut("content") {
                if let Some(text) = content.as_str() {
                    *content = Value::String(format!("{text}\n\n{block}"));
                    return;
                }
            }
        }
    }
    messages.insert(0, json!({"role":"system","content":block}));
}

pub fn estimate_json_messages_tokens(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|message| estimate_text_tokens(&message.to_string()).saturating_add(4))
        .sum::<usize>()
        .saturating_add(messages.len() * 4)
}

fn conversation_chunks(messages: &[StoredMessage], through_ordinal: i64) -> Vec<CandidateChunk> {
    let mut chunks = Vec::new();
    let mut current: Vec<&StoredMessage> = Vec::new();

    for message in messages
        .iter()
        .filter(|message| message.ordinal <= through_ordinal)
    {
        let role = message
            .message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or(&message.role);
        if role == "user" && !current.is_empty() {
            push_chunk(&mut chunks, &current);
            current.clear();
        }
        current.push(message);
    }
    if !current.is_empty() {
        push_chunk(&mut chunks, &current);
    }
    chunks
}

fn push_chunk(chunks: &mut Vec<CandidateChunk>, messages: &[&StoredMessage]) {
    let Some(first) = messages.first() else {
        return;
    };
    let Some(last) = messages.last() else {
        return;
    };
    let text = messages
        .iter()
        .map(|message| {
            let role = message
                .message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or(&message.role);
            format!("{role}: {}", message_text(&message.message))
        })
        .filter(|line| !line.trim_end_matches(':').trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return;
    }
    let terms = tokenize(&text);
    if terms.is_empty() {
        return;
    }
    chunks.push(CandidateChunk {
        start_ordinal: first.ordinal,
        end_ordinal: last.ordinal,
        text,
        terms,
    });
}

fn document_frequency(candidates: &[CandidateChunk]) -> HashMap<String, usize> {
    let mut frequencies = HashMap::new();
    for candidate in candidates {
        let unique = candidate.terms.iter().cloned().collect::<HashSet<_>>();
        for term in unique {
            *frequencies.entry(term).or_insert(0) += 1;
        }
    }
    frequencies
}

fn score_candidate(
    candidate: &CandidateChunk,
    query_terms: &[String],
    query_bigrams: &HashSet<String>,
    document_frequency: &HashMap<String, usize>,
    document_count: f64,
    max_ordinal: f64,
) -> f64 {
    let mut term_frequency: HashMap<&str, usize> = HashMap::new();
    for term in &candidate.terms {
        *term_frequency.entry(term.as_str()).or_insert(0) += 1;
    }

    let unique_query = query_terms.iter().collect::<HashSet<_>>();
    let mut lexical = 0.0;
    let mut overlap = 0usize;
    for term in unique_query {
        let Some(tf) = term_frequency.get(term.as_str()) else {
            continue;
        };
        overlap += 1;
        let df = *document_frequency.get(term).unwrap_or(&1) as f64;
        let idf = ((document_count + 1.0) / (df + 0.5)).ln().max(0.15);
        let tf_weight = 1.0 + (*tf as f64).ln();
        lexical += idf * tf_weight;
    }

    if overlap == 0 {
        return 0.0;
    }

    let candidate_bigrams = bigrams(&candidate.terms);
    let bigram_matches = query_bigrams.intersection(&candidate_bigrams).count() as f64;
    let phrase_bonus = if query_bigrams.is_empty() {
        0.0
    } else {
        bigram_matches / query_bigrams.len() as f64
    };
    let coverage = overlap as f64 / query_terms.iter().collect::<HashSet<_>>().len().max(1) as f64;
    let recency = candidate.end_ordinal as f64 / max_ordinal;

    lexical + coverage * 1.5 + phrase_bonus * 2.0 + recency * 0.15
}

fn bigrams(terms: &[String]) -> HashSet<String> {
    terms
        .windows(2)
        .map(|window| format!("{} {}", window[0], window[1]))
        .collect()
}

fn tokenize(text: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "do",
        "does", "for", "from", "had", "has", "have", "how", "i", "if", "in", "is", "it",
        "its", "me", "my", "of", "on", "or", "our", "so", "that", "the", "their", "this",
        "to", "was", "we", "were", "what", "when", "where", "which", "who", "why", "will",
        "with", "you", "your", "mình", "cho", "của", "là", "và", "với", "có", "những",
        "này", "đó", "thì", "được", "hãy", "về", "một", "các", "trong", "khi", "nào",
    ];
    let stop = STOP_WORDS.iter().copied().collect::<HashSet<_>>();
    text.to_lowercase()
        .split(|ch: char| {
            !ch.is_alphanumeric() && ch != '_' && ch != '-' && ch != '.' && ch != '/'
        })
        .map(|term| term.trim_matches(|ch: char| ch == '.' || ch == '/' || ch == '-'))
        .filter(|term| term.chars().count() >= 2 && !stop.contains(*term))
        .map(ToString::to_string)
        .collect()
}

fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => message
            .get("tool_calls")
            .map(Value::to_string)
            .unwrap_or_default(),
    }
}

fn estimate_text_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4).max(1)
}

fn truncate_to_token_budget(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn preview(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut result = compact
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::{
        augment_messages_with_retrieval, estimate_json_messages_tokens,
        retrieve_relevant_history,
    };
    use crate::conversation::StoredMessage;
    use serde_json::{json, Value};

    fn stored(ordinal: i64, role: &str, content: &str) -> StoredMessage {
        StoredMessage {
            id: format!("msg_{ordinal}"),
            role: role.to_string(),
            message: json!({"role":role,"content":content}),
            model: None,
            route_id: None,
            ordinal,
            created_at: "2026-09-03 00:00:00".to_string(),
        }
    }

    #[test]
    fn retrieves_relevant_old_turn_and_ignores_recent_region() {
        let messages = vec![
            stored(
                1,
                "user",
                "We use optimistic locking with a version column for invoice updates",
            ),
            stored(
                2,
                "assistant",
                "Conflicts return HTTP 409 and the caller retries",
            ),
            stored(3, "user", "Kafka uses zero copy with sendfile"),
            stored(
                4,
                "assistant",
                "That reduces copies between kernel and user space",
            ),
            stored(5, "user", "Recent unrelated turn"),
            stored(6, "assistant", "Recent response"),
        ];
        let query: Value = json!({"role":"user","content":"Remind me how invoice optimistic locking conflicts are handled"});
        let result = retrieve_relevant_history(&messages, 4, &query, 2, 300, 0.2);
        assert!(!result.chunks.is_empty());
        assert_eq!(result.chunks[0].start_ordinal, 1);
        assert_eq!(result.chunks[0].end_ordinal, 2);
        assert!(result.rendered.contains("HTTP 409"));
        assert!(!result.rendered.contains("Recent unrelated"));
    }

    #[test]
    fn empty_overlap_returns_no_retrieval() {
        let messages = vec![stored(1, "user", "Kafka sendfile zero copy")];
        let query = json!({"role":"user","content":"Tell me about tomato gardening"});
        let result = retrieve_relevant_history(&messages, 1, &query, 3, 300, 0.2);
        assert!(result.chunks.is_empty());
        assert!(result.rendered.is_empty());
    }

    #[test]
    fn augmentation_merges_into_existing_memory_system_message() {
        let history = vec![stored(1, "user", "invoice optimistic locking HTTP 409")];
        let query = json!({"role":"user","content":"invoice locking conflict"});
        let retrieval = retrieve_relevant_history(&history, 1, &query, 1, 200, 0.1);
        let mut messages = vec![
            json!({"role":"system","content":"durable memory"}),
            query,
        ];
        let before = estimate_json_messages_tokens(&messages);
        augment_messages_with_retrieval(&mut messages, &retrieval);
        assert_eq!(messages.len(), 2);
        let content = messages[0].get("content").and_then(Value::as_str).unwrap();
        assert!(content.contains("durable memory"));
        assert!(content.contains("Retrieved earlier transcript"));
        assert!(estimate_json_messages_tokens(&messages) > before);
    }
}
