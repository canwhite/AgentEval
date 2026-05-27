use reqwest::Client;
use serde_json::Value;

use crate::config::GraderConfig;
use super::types::JudgeScores;

/// 调用评测 LLM，返回 task_completion 和 response_quality 评分
pub async fn judge(
    config: &GraderConfig,
    prompt: &str,
) -> Result<JudgeScores, String> {
    let client = Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| format!("judge client build error: {}", e))?;

    let url = format!("{}/v1/chat/completions", config.judge_api_base);

    let body = serde_json::json!({
        "model": config.judge_model,
        "messages": [
            { "role": "user", "content": prompt }
        ],
        "temperature": 0.1,
        "max_tokens": 300,
        "stream": false
    });

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.judge_api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("judge request error: {}", e))?;

    let status = resp.status();
    let raw: Value = resp
        .json()
        .await
        .map_err(|e| format!("judge response parse error: {}", e))?;

    if !status.is_success() {
        return Err(format!("judge API returned {}: {}", status.as_u16(), raw));
    }

    // 提取 choices[0].message.content
    let content = raw
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| format!("unexpected judge response structure: {}", raw))?;

    // 解析 LLM 输出的 JSON（可能夹在 markdown 代码块里）
    let json_str = extract_json_block(content);

    serde_json::from_str::<JudgeScores>(&json_str)
        .map_err(|e| format!("judge score parse error from '{}': {}", json_str, e))
}

/// 从 LLM 输出中提取 JSON（可能被 ```json ... ``` 包裹）
fn extract_json_block(text: &str) -> String {
    let text = text.trim();

    // 尝试整体解析
    if text.starts_with('{') {
        return text.to_string();
    }

    // 尝试提取 ```json ... ``` 代码块
    if let Some(start) = text.find("```json") {
        let after_start = &text[start + 7..];
        if let Some(end) = after_start.find("```") {
            return after_start[..end].trim().to_string();
        }
    }

    // 尝试提取 ``` ... ```（无语言标记）
    if let Some(start) = text.find("```") {
        let after_start = &text[start + 3..];
        if let Some(end) = after_start.find("```") {
            return after_start[..end].trim().to_string();
        }
    }

    // 尝试找到第一个 { 到最后一个 }
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            return text[start..=end].to_string();
        }
    }

    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_plain() {
        let result = extract_json_block(r#"{"task_completion": {"score": 0.9, "reason": "ok"}, "response_quality": {"score": 0.8, "reason": "good"}}"#);
        assert!(result.starts_with('{'));
        assert!(result.contains("task_completion"));
    }

    #[test]
    fn test_extract_json_code_fence() {
        let result = extract_json_block("```json\n{\"task_completion\": {\"score\": 0.9, \"reason\": \"ok\"}, \"response_quality\": {\"score\": 0.8, \"reason\": \"good\"}}\n```");
        assert!(result.starts_with('{'));
        assert!(!result.contains("```"));
    }
}
