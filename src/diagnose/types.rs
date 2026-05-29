use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnoseReport {
    pub session_id: String,
    pub diagnosed_at: String,
    pub summary: DiagnoseSummary,
    pub issues: Vec<DiagnoseIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnoseSummary {
    pub total_issues: usize,
    pub errors: usize,
    pub warnings: usize,
    pub infos: usize,
}

impl DiagnoseSummary {
    pub fn from_issues(issues: &[DiagnoseIssue]) -> Self {
        let errors = issues.iter().filter(|i| i.severity == Severity::Error).count();
        let warnings = issues.iter().filter(|i| i.severity == Severity::Warn).count();
        let infos = issues.iter().filter(|i| i.severity == Severity::Info).count();
        Self {
            total_issues: issues.len(),
            errors,
            warnings,
            infos,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnoseIssue {
    pub category: IssueCategory,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    pub location: IssueLocation,
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueLocation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonl_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_index: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IssueCategory {
    Tool,
    Prompt,
    Token,
    View,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Severity {
    Error,
    Warn,
    Info,
}
