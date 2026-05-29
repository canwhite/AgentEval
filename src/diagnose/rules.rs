use std::collections::HashMap;

use serde_json::Value;

use crate::eval::types::{SessionView, Step};

use super::types::*;

/// A single JSONL entry (one API request/response pair).
pub struct JsonlEntry {
    pub id: u64,
    pub request_body: Value,
    pub response_body: Value,
}

// ── Rule entry point ──

pub fn run_all(view: &SessionView, jsonl_entries: &[JsonlEntry]) -> Vec<DiagnoseIssue> {
    let mut issues = Vec::new();

    // Tool rules — operate on view.json turns/steps
    issues.extend(tool_result_missing(view));
    issues.extend(tool_result_error(view));
    issues.extend(tool_duplicate_3plus(view));
    issues.extend(tool_result_empty(view));

    // Prompt rules — need raw JSONL request bodies
    issues.extend(prompt_bloat(jsonl_entries));
    issues.extend(prompt_context_overflow(jsonl_entries));

    // Token rules
    issues.extend(token_empty_response(jsonl_entries));
    issues.extend(token_waste(view));
    issues.extend(token_excessive_input(view));

    // View rule — data integrity
    issues.extend(view_mismatch(view));

    issues
}

// ── Tool rules ──

/// tool_call.result is None (not backfilled cross-turn).
fn tool_result_missing(view: &SessionView) -> Vec<DiagnoseIssue> {
    let mut issues = Vec::new();
    for turn in &view.turns {
        for (si, step) in turn.steps.iter().enumerate() {
            if let Step::ToolCall {
                call_id,
                name,
                arguments,
                result,
            } = step
            {
                if result.is_none() {
                    issues.push(DiagnoseIssue {
                        category: IssueCategory::Tool,
                        severity: Severity::Error,
                        title: "tool_result_missing".into(),
                        detail: format!(
                            "Turn {}, Step {}: tool_call '{}' has no matching tool_result. \
                             The tool was invoked but its result never arrived (broken tool chain).",
                            turn.turn_id, si + 1, name
                        ),
                        location: IssueLocation {
                            jsonl_id: view.jsonl_ids.get(turn.turn_id as usize - 1).copied(),
                            turn_id: Some(turn.turn_id),
                            step_index: Some(si),
                        },
                        evidence: serde_json::to_string(&serde_json::json!({
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                        }))
                        .unwrap_or_default(),
                    });
                }
            }
        }
    }
    issues
}

/// tool_result.is_error == true.
fn tool_result_error(view: &SessionView) -> Vec<DiagnoseIssue> {
    let mut issues = Vec::new();
    for turn in &view.turns {
        // Check inline tool results (from this turn's user-provided tool outputs)
        for tr in &turn.tool_results {
            if tr.is_error {
                issues.push(DiagnoseIssue {
                    category: IssueCategory::Tool,
                    severity: Severity::Error,
                    title: "tool_result_error".into(),
                    detail: format!(
                        "Turn {}: tool_result for call_id '{}' indicates an error. \
                         The tool execution failed on the agent side.",
                        turn.turn_id, tr.call_id
                    ),
                    location: IssueLocation {
                        jsonl_id: view.jsonl_ids.get(turn.turn_id as usize - 1).copied(),
                        turn_id: Some(turn.turn_id),
                        step_index: None,
                    },
                    evidence: tr.content.chars().take(200).collect::<String>(),
                });
            }
        }
        // Also check backfilled results on ToolCall steps
        for (si, step) in turn.steps.iter().enumerate() {
            if let Step::ToolCall { result: Some(tr), .. } = step {
                if tr.is_error {
                    issues.push(DiagnoseIssue {
                        category: IssueCategory::Tool,
                        severity: Severity::Error,
                        title: "tool_result_error".into(),
                        detail: format!(
                            "Turn {}, Step {}: backfilled tool_result for call_id '{}' is marked as error.",
                            turn.turn_id, si + 1, tr.call_id
                        ),
                        location: IssueLocation {
                            jsonl_id: view.jsonl_ids.get(turn.turn_id as usize - 1).copied(),
                            turn_id: Some(turn.turn_id),
                            step_index: Some(si),
                        },
                        evidence: tr.content.chars().take(200).collect::<String>(),
                    });
                }
            }
        }
    }
    issues
}

/// Same name + same arguments called ≥ 3 times.
fn tool_duplicate_3plus(view: &SessionView) -> Vec<DiagnoseIssue> {
    let mut call_counts: HashMap<String, Vec<(u64, usize, String)>> = HashMap::new();

    for turn in &view.turns {
        for (si, step) in turn.steps.iter().enumerate() {
            if let Step::ToolCall { name, arguments, .. } = step {
                let args_key = normalize_args(arguments);
                let key = format!("{}|{}", name, args_key);
                call_counts.entry(key).or_default().push((
                    turn.turn_id,
                    si,
                    name.clone(),
                ));
            }
        }
    }

    let mut issues = Vec::new();
    for (_key, occurrences) in &call_counts {
        if occurrences.len() >= 3 {
            let name = &occurrences[0].2;
            let count = occurrences.len();
            issues.push(DiagnoseIssue {
                category: IssueCategory::Tool,
                severity: Severity::Warn,
                title: "tool_duplicate_3plus".into(),
                detail: format!(
                    "Tool '{}' called {} times with identical arguments. \
                     Repeated identical calls waste turns and tokens — \
                     the agent may be stuck in a retry loop or failed to track prior results.",
                    name, count
                ),
                location: IssueLocation {
                    jsonl_id: view
                        .jsonl_ids
                        .get(occurrences[0].0 as usize - 1)
                        .copied(),
                    turn_id: Some(occurrences[0].0),
                    step_index: Some(occurrences[0].1),
                },
                evidence: serde_json::to_string(&serde_json::json!({
                    "tool": name,
                    "count": count,
                    "first_seen": format!("turn {}, step {}", occurrences[0].0, occurrences[0].1 + 1),
                }))
                .unwrap_or_default(),
            });
        }
    }
    issues
}

/// tool_result.content is empty or whitespace-only.
fn tool_result_empty(view: &SessionView) -> Vec<DiagnoseIssue> {
    let mut issues = Vec::new();

    // Check inline tool results
    for turn in &view.turns {
        for tr in &turn.tool_results {
            if tr.content.trim().is_empty() {
                issues.push(DiagnoseIssue {
                    category: IssueCategory::Tool,
                    severity: Severity::Warn,
                    title: "tool_result_empty".into(),
                    detail: format!(
                        "Turn {}: tool_result for call_id '{}' is empty. \
                         The tool ran but produced no output — this may be normal for \
                         write/delete operations, but for read/search operations it \
                         typically means the tool failed silently.",
                        turn.turn_id, tr.call_id
                    ),
                    location: IssueLocation {
                        jsonl_id: view.jsonl_ids.get(turn.turn_id as usize - 1).copied(),
                        turn_id: Some(turn.turn_id),
                        step_index: None,
                    },
                    evidence: String::new(),
                });
            }
        }
        // Also check backfilled results
        for (si, step) in turn.steps.iter().enumerate() {
            if let Step::ToolCall {
                result: Some(tr), ..
            } = step
            {
                if tr.content.trim().is_empty() {
                    issues.push(DiagnoseIssue {
                        category: IssueCategory::Tool,
                        severity: Severity::Warn,
                        title: "tool_result_empty".into(),
                        detail: format!(
                            "Turn {}, Step {}: backfilled tool_result is empty.",
                            turn.turn_id, si + 1
                        ),
                        location: IssueLocation {
                            jsonl_id: view.jsonl_ids.get(turn.turn_id as usize - 1).copied(),
                            turn_id: Some(turn.turn_id),
                            step_index: Some(si),
                        },
                        evidence: String::new(),
                    });
                }
            }
        }
    }
    issues
}

// ── Prompt rules ──

/// System prompt content > 3000 chars.
fn prompt_bloat(jsonl_entries: &[JsonlEntry]) -> Vec<DiagnoseIssue> {
    let mut issues = Vec::new();
    if jsonl_entries.is_empty() {
        return issues;
    }

    // Check the first JSONL entry for this session
    let first = &jsonl_entries[0];
    let messages = first
        .request_body
        .get("messages")
        .and_then(|v| v.as_array());

    if let Some(msgs) = messages {
        for msg in msgs {
            if msg.get("role").and_then(|v| v.as_str()) == Some("system") {
                let content = extract_content(msg);
                let len = content.chars().count();
                if len > 3000 {
                    issues.push(DiagnoseIssue {
                        category: IssueCategory::Prompt,
                        severity: Severity::Warn,
                        title: "prompt_bloat".into(),
                        detail: format!(
                            "System prompt is {} chars (threshold: 3000). \
                             Overly long system instructions waste context window space. \
                             Consider trimming redundant instructions or moving them to \
                             a knowledge base retrieved on demand.",
                            len
                        ),
                        location: IssueLocation {
                            jsonl_id: Some(first.id),
                            turn_id: None,
                            step_index: None,
                        },
                        evidence: content.chars().take(300).collect::<String>(),
                    });
                }
                break; // Only check the first system message
            }
        }
    }
    issues
}

/// Signs of history truncation: tool role message with no matching tool_calls in
/// preceding assistant messages within the same request.
fn prompt_context_overflow(jsonl_entries: &[JsonlEntry]) -> Vec<DiagnoseIssue> {
    let mut issues = Vec::new();

    for entry in jsonl_entries {
        let messages = entry
            .request_body
            .get("messages")
            .and_then(|v| v.as_array());

        let msgs = match messages {
            Some(m) => m,
            None => continue,
        };

        // Collect all tool_call_ids from assistant messages in this request
        let mut declared_call_ids: Vec<String> = Vec::new();
        for msg in msgs.iter() {
            if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tool_calls {
                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                            declared_call_ids.push(id.to_string());
                        }
                    }
                }
            }
        }

        // Check tool messages: their tool_call_id should match a declared call_id
        for msg in msgs.iter() {
            if msg.get("role").and_then(|v| v.as_str()) == Some("tool") {
                if let Some(tc_id) = msg.get("tool_call_id").and_then(|v| v.as_str()) {
                    if !declared_call_ids.contains(&tc_id.to_string()) {
                        issues.push(DiagnoseIssue {
                            category: IssueCategory::Prompt,
                            severity: Severity::Error,
                            title: "prompt_context_overflow".into(),
                            detail: format!(
                                "JSONL id {}: tool message references tool_call_id '{}' \
                                 which has no matching tool_calls in any assistant message \
                                 in this request. The tool call that produced this result \
                                 was likely truncated from conversation history — this is \
                                 a sign of context window overflow.",
                                entry.id, tc_id
                            ),
                            location: IssueLocation {
                                jsonl_id: Some(entry.id),
                                turn_id: None,
                                step_index: None,
                            },
                            evidence: serde_json::to_string(&serde_json::json!({
                                "orphan_tool_call_id": tc_id,
                                "available_call_ids": declared_call_ids,
                            }))
                            .unwrap_or_default(),
                        });
                    }
                }
            }
        }
    }
    issues
}

// ── Token rules ──

/// response_body.choices empty or content empty (handles both SSE string and JSON object).
fn token_empty_response(jsonl_entries: &[JsonlEntry]) -> Vec<DiagnoseIssue> {
    let mut issues = Vec::new();

    for entry in jsonl_entries {
        let is_empty = match &entry.response_body {
            // Streaming (SSE) response: parse the SSE text to check for meaningful content
            Value::String(s) if s.contains("data: ") => {
                !sse_has_content(s)
            }
            // Non-streaming JSON response
            Value::Object(_) => {
                let choices = entry
                    .response_body
                    .get("choices")
                    .and_then(|v| v.as_array());
                match choices {
                    None => true,
                    Some(arr) if arr.is_empty() => true,
                    Some(arr) => {
                        let msg = arr[0].get("message");
                        match msg {
                            None => true,
                            Some(m) => {
                                let has_content = m
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .map(|s| !s.is_empty())
                                    .unwrap_or(false);
                                let has_tool_calls = m
                                    .get("tool_calls")
                                    .and_then(|v| v.as_array())
                                    .map(|a| !a.is_empty())
                                    .unwrap_or(false);
                                !has_content && !has_tool_calls
                            }
                        }
                    }
                }
            }
            // Unknown format — can't determine, skip
            _ => false,
        };

        if is_empty {
            issues.push(DiagnoseIssue {
                category: IssueCategory::Token,
                severity: Severity::Error,
                title: "token_empty_response".into(),
                detail: format!(
                    "JSONL id {}: API response produced no meaningful output — \
                     empty content and no tool calls. Tokens may have been consumed \
                     with nothing useful returned.",
                    entry.id
                ),
                location: IssueLocation {
                    jsonl_id: Some(entry.id),
                    turn_id: None,
                    step_index: None,
                },
                evidence: String::new(),
            });
        }
    }
    issues
}

/// Check if an SSE stream has any meaningful content (text or tool calls).
fn sse_has_content(sse_text: &str) -> bool {
    let mut has_text = false;
    let mut has_tool_call = false;

    for line in sse_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => continue,
        };
        if data == "[DONE]" {
            continue;
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let choices = match chunk.get("choices").and_then(|v| v.as_array()) {
            Some(c) => c,
            None => continue,
        };
        for choice in choices {
            let delta = match choice.get("delta") {
                Some(d) => d,
                None => continue,
            };
            if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                if !c.is_empty() {
                    has_text = true;
                }
            }
            if delta.get("tool_calls").and_then(|v| v.as_array()).is_some() {
                has_tool_call = true;
            }
        }
    }

    has_text || has_tool_call
}

/// output/input < 2% AND input > 5000 tokens.
fn token_waste(view: &SessionView) -> Vec<DiagnoseIssue> {
    let mut issues = Vec::new();

    for turn in &view.turns {
        if let Some(ref usage) = turn.usage {
            if usage.input_tokens > 5000 {
                let ratio = if usage.input_tokens > 0 {
                    usage.output_tokens as f64 / usage.input_tokens as f64
                } else {
                    0.0
                };
                if ratio < 0.02 {
                    issues.push(DiagnoseIssue {
                        category: IssueCategory::Token,
                        severity: Severity::Warn,
                        title: "token_waste".into(),
                        detail: format!(
                            "Turn {}: {} input tokens → {} output tokens (ratio: {:.2}%). \
                             Massive context sent but negligible output produced — the agent \
                             is spending tokens inefficiently on this request.",
                            turn.turn_id,
                            usage.input_tokens,
                            usage.output_tokens,
                            ratio * 100.0
                        ),
                        location: IssueLocation {
                            jsonl_id: view.jsonl_ids.get(turn.turn_id as usize - 1).copied(),
                            turn_id: Some(turn.turn_id),
                            step_index: None,
                        },
                        evidence: serde_json::to_string(&serde_json::json!({
                            "input_tokens": usage.input_tokens,
                            "output_tokens": usage.output_tokens,
                            "ratio_pct": format!("{:.2}", ratio * 100.0),
                        }))
                        .unwrap_or_default(),
                    });
                }
            }
        }
    }
    issues
}

/// input > 30000 AND output < 1000.
fn token_excessive_input(view: &SessionView) -> Vec<DiagnoseIssue> {
    let mut issues = Vec::new();

    for turn in &view.turns {
        if let Some(ref usage) = turn.usage {
            if usage.input_tokens > 30000 && usage.output_tokens < 1000 {
                issues.push(DiagnoseIssue {
                    category: IssueCategory::Token,
                    severity: Severity::Info,
                    title: "token_excessive_input".into(),
                    detail: format!(
                        "Turn {}: {} input tokens, {} output tokens. \
                         Single API call consumed {} input tokens with minimal output — \
                         the context may be overstuffed or the task doesn't need this much context.",
                        turn.turn_id,
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.input_tokens
                    ),
                    location: IssueLocation {
                        jsonl_id: view.jsonl_ids.get(turn.turn_id as usize - 1).copied(),
                        turn_id: Some(turn.turn_id),
                        step_index: None,
                    },
                    evidence: serde_json::to_string(&serde_json::json!({
                        "input_tokens": usage.input_tokens,
                        "output_tokens": usage.output_tokens,
                    }))
                    .unwrap_or_default(),
                });
            }
        }
    }
    issues
}

// ── View rules ──

/// Data integrity: turns.len() != jsonl_ids.len().
fn view_mismatch(view: &SessionView) -> Vec<DiagnoseIssue> {
    if view.turns.len() != view.jsonl_ids.len() {
        vec![DiagnoseIssue {
            category: IssueCategory::View,
            severity: Severity::Warn,
            title: "view_mismatch".into(),
            detail: format!(
                "view.json has {} turns but {} jsonl_ids. Each turn should correspond \
                 to exactly one API request — this mismatch may indicate a bug in session \
                 view construction, not an agent problem.",
                view.turns.len(),
                view.jsonl_ids.len()
            ),
            location: IssueLocation {
                jsonl_id: None,
                turn_id: None,
                step_index: None,
            },
            evidence: serde_json::to_string(&serde_json::json!({
                "turns_count": view.turns.len(),
                "jsonl_ids_count": view.jsonl_ids.len(),
            }))
            .unwrap_or_default(),
        }]
    } else {
        Vec::new()
    }
}

// ── Helpers ──

/// Normalize JSON arguments for comparison: parse + re-serialize with sorted keys.
fn normalize_args(args: &Value) -> String {
    match args {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut pairs = Vec::new();
            for k in keys {
                pairs.push(format!(
                    "{}:{}",
                    serde_json::to_string(k).unwrap_or_default(),
                    normalize_args(&map[k])
                ));
            }
            format!("{{{}}}", pairs.join(","))
        }
        Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(normalize_args).collect();
            format!("[{}]", items.join(","))
        }
        Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
    }
}

/// Extract text content from a message value (content may be string or array of blocks).
fn extract_content(msg: &Value) -> String {
    let content = match msg.get("content") {
        Some(c) => c,
        None => return String::new(),
    };
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    parts.push(t.to_string());
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}
