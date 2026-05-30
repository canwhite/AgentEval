pub mod agent;
pub mod backend;
pub mod prompt;
pub mod tool;
pub mod tools;
pub mod types;

use std::io::Write;

use chrono::Utc;

use crate::config::{GraderConfig, ProbeConfig};
use crate::diagnose;
use crate::eval::types::SessionView;

use agent::AgentLoop;
use backend::OpenAiBackend;
use tool::Registry;
use types::*;

/// Run the probe agent against a session.
///
/// 1. Read diagnose issues from .diagnose.json
/// 2. Read session view from .view.json
/// 3. Build system + user prompts
/// 4. Run the AgentLoop with file tools pointed at the source project
/// 5. Parse the LLM's JSON output into a ProbeReport
/// 6. Write .probe.json
pub async fn run(session_id: &str, log_dir: &str, probe_config: &ProbeConfig, grader_config: &GraderConfig) -> Result<ProbeReport, String> {
    // 1. Read diagnose issues
    let diagnose_report = diagnose::read_existing(session_id, log_dir)
        .map_err(|e| format!("diagnose required before probe: {}", e))?;

    if diagnose_report.issues.is_empty() {
        return Err("no diagnose issues to probe — nothing to investigate".into());
    }

    // 2. Read session view
    let view_path = format!("{}/{}.view.json", log_dir, session_id);
    let view_json = std::fs::read_to_string(&view_path)
        .map_err(|e| format!("failed to read {}: {}", view_path, e))?;
    let view: SessionView = serde_json::from_str(&view_json)
        .map_err(|e| format!("failed to parse {}: {}", view_path, e))?;

    // 3. Verify source project directory
    let source_dir = probe_config.source_project_dir.trim();
    if source_dir.is_empty() {
        return Err("PROBE_SOURCE_PROJECT_DIR is not set".into());
    }
    let source_meta = std::fs::metadata(source_dir)
        .map_err(|e| format!("PROBE_SOURCE_PROJECT_DIR '{}' not accessible: {}", source_dir, e))?;
    if !source_meta.is_dir() {
        return Err(format!("PROBE_SOURCE_PROJECT_DIR '{}' is not a directory", source_dir));
    }

    // 4. Build prompts
    let system_prompt = prompt::build_system_prompt();
    let user_prompt = prompt::build_user_prompt(&diagnose_report.issues, &view);

    // 5. Set up tools
    let mut registry = Registry::new();
    registry.register(Box::new(tools::ReadFile::new(source_dir)));
    registry.register(Box::new(tools::Grep::new(source_dir)));
    registry.register(Box::new(tools::ListDir::new(source_dir)));
    registry.register(Box::new(tools::Glob::new(source_dir)));

    // 6. Set up backend — reuse grader/judge config
    if grader_config.judge_api_key.is_empty() {
        return Err("JUDGE_API_KEY is not set".into());
    }
    let backend = OpenAiBackend::new(
        &grader_config.judge_api_base,
        &grader_config.judge_model,
        &grader_config.judge_api_key,
    );

    // 7. Run agent loop
    let mut agent = AgentLoop::new(backend, registry, &system_prompt, &user_prompt, 30);
    let raw_output = agent.run().await?;

    // 8. Parse output
    eprintln!("[probe] parsing LLM output ({} chars)...", raw_output.len());
    let report = parse_probe_output(session_id, &raw_output);
    eprintln!(
        "[probe] parsed: {} findings + {} additional, overall={:.50}...",
        report.findings.len(),
        report.additional_findings.len(),
        report.overall_assessment
    );

    // 9. Write .probe.json
    let probe_path = format!("{}/{}.probe.json", log_dir, session_id);
    let json = serde_json::to_string_pretty(&report)
        .map_err(|e| format!("failed to serialize probe report: {}", e))?;
    eprintln!("[probe] writing {} bytes to {}...", json.len(), probe_path);
    let mut file = std::fs::File::create(&probe_path)
        .map_err(|e| format!("failed to create {}: {}", probe_path, e))?;
    file.write_all(json.as_bytes())
        .map_err(|e| format!("failed to write {}: {}", probe_path, e))?;
    eprintln!("[probe] report written successfully.");

    Ok(report)
}

/// Read an existing .probe.json.
pub fn read_existing(session_id: &str, log_dir: &str) -> Result<ProbeReport, String> {
    let probe_path = format!("{}/{}.probe.json", log_dir, session_id);
    let json = std::fs::read_to_string(&probe_path)
        .map_err(|e| format!("probe not found for {}: {}", session_id, e))?;
    serde_json::from_str(&json)
        .map_err(|e| format!("failed to parse {}: {}", probe_path, e))
}

/// Parse the LLM's raw output into a ProbeReport.
///
/// Tries strict JSON parsing first, then falls back to extracting a JSON block
/// from markdown-wrapped output (```json ... ```).
fn parse_probe_output(session_id: &str, raw: &str) -> ProbeReport {
    let raw = raw.trim();

    // Try direct JSON parse first
    if let Ok(report) = serde_json::from_str::<ProbeReport>(raw) {
        return report;
    }

    // Fallback: extract JSON block from markdown
    let json_str = extract_json_block(raw).unwrap_or(raw);

    match serde_json::from_str::<ProbeReport>(json_str) {
        Ok(mut report) => {
            report.session_id = session_id.to_string();
            report.probed_at = Utc::now().to_rfc3339();
            report
        }
        Err(e) => {
            // Return a report with the parse error and raw output preserved
            ProbeReport {
                session_id: session_id.to_string(),
                probed_at: Utc::now().to_rfc3339(),
                findings: Vec::new(),
                additional_findings: Vec::new(),
                overall_assessment: format!(
                    "Failed to parse probe output as JSON: {}. Raw output preserved in parse_error.",
                    e
                ),
                parse_error: Some(raw.to_string()),
            }
        }
    }
}

/// Extract a JSON block from text that may be wrapped in markdown fences.
fn extract_json_block(text: &str) -> Option<&str> {
    // Look for ```json ... ``` or ``` ... ```
    if let Some(start) = text.find("```json") {
        let after_start = &text[start + 7..];
        if let Some(end) = after_start.find("```") {
            return Some(after_start[..end].trim());
        }
    }
    if let Some(start) = text.find("```") {
        let after_start = &text[start + 3..];
        if let Some(end) = after_start.find("```") {
            return Some(after_start[..end].trim());
        }
    }
    // Look for bare JSON object
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            return Some(&text[start..=end]);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_block_from_markdown() {
        let input = "Here is the report:\n```json\n{\"findings\": [], \"overall_assessment\": \"ok\"}\n```\nDone.";
        let result = extract_json_block(input);
        assert!(result.is_some());
        assert!(result.unwrap().contains("\"findings\""));
    }

    #[test]
    fn test_extract_json_block_bare() {
        let input = "{\"findings\": [], \"overall_assessment\": \"ok\"}";
        let result = extract_json_block(input);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_probe_output_fallback() {
        let raw = "```json\n{\"findings\": [], \"additional_findings\": [], \"overall_assessment\": \"test\"}\n```";
        let report = parse_probe_output("test_session", raw);
        assert_eq!(report.overall_assessment, "test");
    }

    #[test]
    fn test_parse_probe_output_parse_error() {
        let raw = "This is not JSON at all.";
        let report = parse_probe_output("test_session", raw);
        assert!(report.parse_error.is_some());
        assert!(report.overall_assessment.contains("Failed to parse"));
    }
}
