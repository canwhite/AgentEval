pub mod rules;
pub mod types;

use std::io::Write;

use chrono::Utc;

use crate::config::GraderConfig;
use crate::eval::types::SessionView;
use rules::JsonlEntry;
use types::*;

/// Run all diagnostic rules against a session.
///
/// Reads `{log_dir}/{session_id}.view.json` and the corresponding `.jsonl`,
/// runs all 10 rules, writes `{log_dir}/{session_id}.diagnose.json`,
/// and returns the report.
pub async fn run(session_id: &str, log_dir: &str, grader_config: &GraderConfig) -> Result<DiagnoseReport, String> {
    // 1. Read view.json
    let view_path = format!("{}/{}.view.json", log_dir, session_id);
    let view_json = std::fs::read_to_string(&view_path)
        .map_err(|e| format!("failed to read {}: {}", view_path, e))?;
    let view: SessionView = serde_json::from_str(&view_json)
        .map_err(|e| format!("failed to parse {}: {}", view_path, e))?;

    // 2. Derive jsonl_stem from session_id (split on LAST '_')
    let jsonl_stem = match session_id.rfind('_') {
        Some(pos) => &session_id[..pos],
        None => return Err(format!("invalid session_id (no underscore): {}", session_id)),
    };
    let jsonl_path = format!("{}/{}.jsonl", log_dir, jsonl_stem);

    // 3. Read relevant JSONL entries
    let jsonl_entries = read_jsonl_entries(&jsonl_path, &view.jsonl_ids)?;

    // 4. Run all rules
    let issues = rules::run_all(&view, &jsonl_entries);

    // 5. Generate LLM summary (best-effort, skipped if no API key or no issues)
    let llm_summary = summarize_issues(grader_config, &issues).await;

    // 6. Build report
    let summary = DiagnoseSummary::from_issues(&issues);
    let report = DiagnoseReport {
        session_id: session_id.to_string(),
        diagnosed_at: Utc::now().to_rfc3339(),
        summary,
        issues,
        llm_summary,
    };

    // 7. Write .diagnose.json
    let diagnose_path = format!("{}/{}.diagnose.json", log_dir, session_id);
    let json = serde_json::to_string_pretty(&report)
        .map_err(|e| format!("failed to serialize report: {}", e))?;
    let mut file = std::fs::File::create(&diagnose_path)
        .map_err(|e| format!("failed to create {}: {}", diagnose_path, e))?;
    file.write_all(json.as_bytes())
        .map_err(|e| format!("failed to write {}: {}", diagnose_path, e))?;

    Ok(report)
}

/// Read an existing .diagnose.json.
pub fn read_existing(session_id: &str, log_dir: &str) -> Result<DiagnoseReport, String> {
    let diagnose_path = format!("{}/{}.diagnose.json", log_dir, session_id);
    let json = std::fs::read_to_string(&diagnose_path)
        .map_err(|e| format!("diagnose not found for {}: {}", session_id, e))?;
    serde_json::from_str(&json)
        .map_err(|e| format!("failed to parse {}: {}", diagnose_path, e))
}

/// Call the judge LLM to generate a 2-3 sentence summary of diagnose issues.
/// Returns None if no API key is configured, there are no issues, or the LLM call fails.
async fn summarize_issues(config: &GraderConfig, issues: &[DiagnoseIssue]) -> Option<String> {
    if config.judge_api_key.is_empty() || issues.is_empty() {
        return None;
    }

    // Build prompt listing all issues
    let mut issues_text = String::new();
    for issue in issues {
        issues_text.push_str(&format!(
            "- [{:?}] {}: {}\n",
            issue.severity, issue.title, issue.detail
        ));
    }

    let prompt = format!(
        "You are an agent evaluation expert. Below are issues found by automated \
         diagnosis of an AI agent session. Write a 2-3 sentence summary of the key \
         problems found and their likely impact on the session quality. Be concise.\n\n\
         Issues:\n{}",
        issues_text
    );

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[diagnose] failed to build HTTP client for summary: {}", e);
            return None;
        }
    };

    let url = format!("{}/chat/completions", config.judge_api_base.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": config.judge_model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0.3,
        "max_tokens": 200,
        "stream": false,
    });

    let resp = match client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.judge_api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[diagnose] LLM summary request failed: {}", e);
            return None;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        eprintln!("[diagnose] LLM summary API error {}: {}", status, text);
        return None;
    }

    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[diagnose] failed to read LLM summary response: {}", e);
            return None;
        }
    };

    // Parse choices[0].message.content
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[diagnose] failed to parse LLM summary JSON: {}", e);
            return None;
        }
    };

    let content = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.trim().to_string());

    if let Some(ref summary) = content {
        eprintln!("[diagnose] LLM summary generated ({} chars)", summary.len());
    }

    content
}

/// Read specific JSONL entries by their `id` fields.
fn read_jsonl_entries(jsonl_path: &str, ids: &[u64]) -> Result<Vec<JsonlEntry>, String> {
    let text = std::fs::read_to_string(jsonl_path)
        .map_err(|e| format!("failed to read {}: {}", jsonl_path, e))?;

    let id_set: std::collections::HashSet<u64> = ids.iter().copied().collect();
    let mut entries = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            format!("failed to parse JSONL line in {}: {}", jsonl_path, e)
        })?;

        let entry_id = entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        if id_set.contains(&entry_id) {
            entries.push(JsonlEntry {
                id: entry_id,
                request_body: entry
                    .get("request_body")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                response_body: entry
                    .get("response_body")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            });
        }
    }

    Ok(entries)
}

/// Read raw JSONL lines by IDs (for the raw JSONL API).
pub fn read_raw_jsonl(jsonl_stem: &str, ids: &[u64], log_dir: &str) -> Result<Vec<serde_json::Value>, String> {
    let jsonl_path = format!("{}/{}.jsonl", log_dir, jsonl_stem);
    let text = std::fs::read_to_string(&jsonl_path)
        .map_err(|e| format!("failed to read {}: {}", jsonl_path, e))?;

    let id_set: std::collections::HashSet<u64> = ids.iter().copied().collect();
    let mut results = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: serde_json::Value =
            serde_json::from_str(line).unwrap_or(serde_json::Value::Null);
        let entry_id = entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        if id_set.contains(&entry_id) {
            results.push(entry);
        }
    }

    Ok(results)
}
