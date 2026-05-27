use serde_json::Value;

use crate::eval::types::Step;

/// 从 request body 提取 messages 数组
pub fn parse_request_messages(body: &Value) -> Vec<Value> {
    body.get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// 从 response body 提取 model 步骤
///
/// 自动区分非流式（JSON）和流式（SSE 文本）响应。
pub fn parse_response_steps(body: &Value) -> Vec<Step> {
    match body {
        Value::Object(_) => parse_non_streaming(body),
        Value::String(s) if s.contains("data: ") => parse_sse(s),
        _ => vec![],
    }
}

/// 从 response body 提取 usage 信息
pub fn parse_response_usage(body: &Value) -> Option<super::super::eval::types::Usage> {
    match body {
        Value::Object(_) => {
            let usage = body.get("usage")?;
            Some(super::super::eval::types::Usage {
                input_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            })
        }
        Value::String(s) if s.contains("data: ") => {
            // 流式响应从最后一个有 usage 的 chunk 提取
            let mut last_usage = None;
            for line in s.lines() {
                let line = line.trim();
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        continue;
                    }
                    if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                        if let Some(usage) = chunk.get("usage") {
                            last_usage = Some(super::super::eval::types::Usage {
                                input_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                output_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                            });
                        }
                    }
                }
            }
            last_usage
        }
        _ => None,
    }
}

// ── 非流式解析 ──

fn parse_non_streaming(body: &Value) -> Vec<Step> {
    let mut steps = Vec::new();

    let choices = match body.get("choices").and_then(|v| v.as_array()) {
        Some(c) => c,
        None => return steps,
    };

    let message = match choices.first().and_then(|c| c.get("message")) {
        Some(m) => m,
        None => return steps,
    };

    // 1. reasoning content（如果模型返回）
    if let Some(reasoning) = message
        .get("reasoning_content")
        .and_then(|v| v.as_str())
    {
        if !reasoning.is_empty() {
            steps.push(Step::Reasoning {
                content: reasoning.to_string(),
            });
        }
    }

    // 2. tool calls（先于 text，因为模型通常先思考/调用工具再总结）
    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tool_calls {
            let call_id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let func = tc.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let arguments = func
                .and_then(|f| f.get("arguments"))
                .map(|v| {
                    // arguments 可能是 JSON 字符串或已解析的对象
                    if let Some(s) = v.as_str() {
                        serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
                    } else {
                        v.clone()
                    }
                })
                .unwrap_or(Value::Null);

            steps.push(Step::ToolCall {
                call_id,
                name,
                arguments,
                result: None,
            });
        }
    }

    // 3. text content
    let content = message.get("content");
    if let Some(s) = content.and_then(|v| v.as_str()) {
        if !s.is_empty() {
            steps.push(Step::Text {
                content: s.to_string(),
            });
        }
    }

    steps
}

// ── SSE 流式解析 ──

#[derive(Default)]
struct SseToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn parse_sse(sse_text: &str) -> Vec<Step> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<SseToolCall> = Vec::new();

    for line in sse_text.lines() {
        let line = line.trim();
        // 跳过空行、注释、非 data 行
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

            // 累积 content
            if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                content.push_str(c);
            }
            // 累积 reasoning
            if let Some(r) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                reasoning.push_str(r);
            }

            // 累积 tool_calls（按 index 分片）
            if let Some(tc_array) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tc_array {
                    let idx = tc
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    while tool_calls.len() <= idx {
                        tool_calls.push(SseToolCall::default());
                    }

                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        if !id.is_empty() {
                            tool_calls[idx].id = id.to_string();
                        }
                    }
                    if let Some(func) = tc.get("function") {
                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                            if !name.is_empty() {
                                tool_calls[idx].name = name.to_string();
                            }
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                            tool_calls[idx].arguments.push_str(args);
                        }
                    }
                }
            }
        }
    }

    // 组装 steps：reasoning → tool_calls → text
    let mut steps = Vec::new();

    if !reasoning.is_empty() {
        steps.push(Step::Reasoning { content: reasoning });
    }

    for tc in &tool_calls {
        if tc.name.is_empty() {
            continue;
        }
        let arguments = serde_json::from_str(&tc.arguments)
            .unwrap_or_else(|_| Value::String(tc.arguments.clone()));
        steps.push(Step::ToolCall {
            call_id: tc.id.clone(),
            name: tc.name.clone(),
            arguments,
            result: None,
        });
    }

    if !content.is_empty() {
        steps.push(Step::Text { content });
    }

    steps
}

// ── 辅助：提取消息的文本内容 ──

/// 从 message value 提取可读文本（content 可能是 string 或 array）
pub fn extract_text_content(msg: &Value) -> String {
    let content = match msg.get("content") {
        Some(c) => c,
        None => return String::new(),
    };

    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if let Some(t) = block.get("type").and_then(|v| v.as_str()) {
                    match t {
                        "text" => {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                parts.push(text.to_string());
                            }
                        }
                        "image_url" => {
                            parts.push("[image]".to_string());
                        }
                        _ => {
                            parts.push(format!("[{}]", t));
                        }
                    }
                }
            }
            parts.join("\n")
        }
        Value::Null => String::new(),
        _ => content.to_string(),
    }
}
