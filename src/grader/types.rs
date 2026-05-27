use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 规则统计产出的定量指标
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub total_tool_calls: usize,
    pub tool_success_count: usize,
    pub tool_error_count: usize,
    pub duplicate_tool_calls: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub avg_duration_ms: u64,
    pub turn_count: usize,
    pub final_text_len: usize,
    pub has_final_text: bool,
    pub has_tool_error: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GradeReport {
    pub session_id: String,
    pub model: String,
    #[serde(default)]
    pub jsonl_ids: Vec<u64>,
    pub turn_count: usize,
    pub dimensions: Vec<DimensionScore>,
    pub overall: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DimensionScore {
    pub metric: String,
    pub score: f64,
    pub source: String, // "rule" | "llm"
    pub reason: String,
    pub details: Value,
    pub weight: f64,
}

/// LLM 评审返回的原始评分
#[derive(Debug, Clone, Deserialize)]
pub struct JudgeScores {
    pub task_completion: JudgeScoreItem,
    pub response_quality: JudgeScoreItem,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JudgeScoreItem {
    pub score: f64,
    pub reason: String,
}
