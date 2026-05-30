//! Types for the probe module — shared between agent loop, tools, and output.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single finding from the probe agent's configuration review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeFinding {
    pub issue_title: String,
    pub category: String,
    pub root_cause: String,
    pub affected_files: Vec<String>,
    pub confidence: String,
    pub recommendation: String,
    pub evidence: String,
}

/// The complete probe report, written to .probe.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeReport {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub probed_at: String,
    #[serde(default)]
    pub findings: Vec<ProbeFinding>,
    #[serde(default)]
    pub additional_findings: Vec<ProbeFinding>,
    #[serde(default)]
    pub overall_assessment: String,
    /// When JSON parsing failed, contains the raw LLM output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
}

/// Summary of probe results shown in dashboard list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeSummary {
    pub total_findings: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
}

impl ProbeSummary {
    pub fn from_report(report: &ProbeReport) -> Self {
        let all: Vec<&ProbeFinding> = report
            .findings
            .iter()
            .chain(report.additional_findings.iter())
            .collect();
        let high = all.iter().filter(|f| f.confidence == "high").count();
        let medium = all.iter().filter(|f| f.confidence == "medium").count();
        let low = all.iter().filter(|f| f.confidence == "low").count();
        Self {
            total_findings: all.len(),
            high,
            medium,
            low,
        }
    }
}

// ── Agent Loop types ──

/// A conversation message in OpenAI-flavored internal format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    pub fn system(content: &str) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn user(content: &str) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant_with_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            name: None,
        }
    }

    pub fn tool_result(tool_call_id: &str, content: &str) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.to_string()),
            name: None,
        }
    }

    /// Check if this is an assistant message with tool calls.
    #[allow(dead_code)]
    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls
            .as_ref()
            .map(|tc| !tc.is_empty())
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Parsed response from the LLM backend.
pub struct LlmResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    #[allow(dead_code)]
    pub usage: Option<UsageStats>,
}

impl LlmResponse {
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// Token usage reported by the backend for a single inference call.
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct UsageStats {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// Convert an OpenAI chat completion response body into an LlmResponse.
///
/// Handles both streaming (SSE) and non-streaming (JSON) responses.
pub fn parse_llm_response(body: &str) -> Result<LlmResponse, String> {
    if body.trim_start().starts_with("data:") || body.contains("data: ") {
        parse_sse_response(body)
    } else {
        parse_json_response(body)
    }
}

fn parse_json_response(body: &str) -> Result<LlmResponse, String> {
    let v: Value =
        serde_json::from_str(body).map_err(|e| format!("failed to parse LLM JSON response: {}", e))?;

    let choices = v
        .get("choices")
        .and_then(|c| c.as_array())
        .ok_or("missing choices array")?;

    let first = choices
        .first()
        .ok_or("empty choices")?;

    let msg = first.get("message").ok_or("missing message in choice")?;

    let content = msg
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();

    let tool_calls = msg
        .get("tool_calls")
        .and_then(|tc| tc.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    Some(ToolCall {
                        id: tc.get("id")?.as_str()?.to_string(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: tc.get("function")?.get("name")?.as_str()?.to_string(),
                            arguments: tc
                                .get("function")?
                                .get("arguments")?
                                .as_str()
                                .unwrap_or("{}")
                                .to_string(),
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let usage = v.get("usage").map(|u| UsageStats {
        prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        completion_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
    });

    Ok(LlmResponse {
        content,
        tool_calls,
        usage,
    })
}

fn parse_sse_response(sse_text: &str) -> Result<LlmResponse, String> {
    let mut content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut last_usage: Option<UsageStats> = None;

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

        if let Some(usage) = chunk.get("usage") {
            last_usage = Some(UsageStats {
                prompt_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                completion_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            });
        }

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
                content.push_str(c);
            }

            if let Some(tc_arr) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tc_arr {
                    let idx = tc
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;

                    while tool_calls.len() <= idx {
                        tool_calls.push(ToolCall {
                            id: String::new(),
                            call_type: "function".into(),
                            function: FunctionCall {
                                name: String::new(),
                                arguments: String::new(),
                            },
                        });
                    }

                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        if !id.is_empty() {
                            tool_calls[idx].id = id.to_string();
                        }
                    }
                    if let Some(func) = tc.get("function") {
                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                            if !name.is_empty() {
                                tool_calls[idx].function.name = name.to_string();
                            }
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                            tool_calls[idx].function.arguments.push_str(args);
                        }
                    }
                }
            }
        }
    }

    // Filter incomplete tool calls
    tool_calls.retain(|tc| !tc.id.is_empty() && !tc.function.name.is_empty());

    Ok(LlmResponse {
        content,
        tool_calls,
        usage: last_usage,
    })
}
