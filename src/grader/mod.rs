pub mod types;
pub mod rules;
pub mod prompt;
pub mod judge;

use serde_json::json;

use crate::config::GraderConfig;
use crate::eval::types::SessionView;
use types::{DimensionScore, GradeReport};

/// 运行完整的 grading pipeline:
///   1. 规则统计 → MetricsSnapshot
///   2. LLM 评审 → task_completion + response_quality
///   3. 汇总加权 → GradeReport
pub async fn run_pipeline(view: &SessionView, config: &GraderConfig) -> GradeReport {
    let metrics = rules::extract_metrics(view);

    // Step 1 & 2: LLM 评审（异步）
    let (tc_score, tc_reason, rq_score, rq_reason) = match judge_llm(view, &metrics, config).await {
        Ok(scores) => (
            scores.task_completion.score.clamp(0.0, 1.0),
            scores.task_completion.reason,
            scores.response_quality.score.clamp(0.0, 1.0),
            scores.response_quality.reason,
        ),
        Err(e) => {
            eprintln!("[grader] LLM judge failed: {}, falling back to rule-based", e);
            // 降级：用规则猜完成度
            let tc = if metrics.has_final_text && !metrics.has_tool_error {
                (0.7, "有文本回复且无错误（降级估算）".to_string())
            } else if metrics.has_tool_error {
                (0.3, "存在工具错误（降级估算）".to_string())
            } else {
                (0.4, "无明显终态（降级估算）".to_string())
            };
            let rq = if metrics.final_text_len > 100 {
                (0.6, "回复篇幅尚可（降级估算）".to_string())
            } else {
                (0.3, "回复较短或无回复（降级估算）".to_string())
            };
            (tc.0, tc.1, rq.0, rq.1)
        }
    };

    // Step 3: 规则维度
    let (tool_score, tool_reason) = rules::calc_tool_efficiency(&metrics);
    let (perf_score, perf_reason) = rules::calc_performance(&metrics);

    let dimensions = vec![
        DimensionScore {
            metric: "task_completion".into(),
            score: tc_score,
            source: "llm".into(),
            reason: tc_reason,
            details: json!({}),
            weight: 0.35,
        },
        DimensionScore {
            metric: "tool_efficiency".into(),
            score: tool_score,
            source: "rule".into(),
            reason: tool_reason,
            details: json!({
                "total_calls": metrics.total_tool_calls,
                "error_count": metrics.tool_error_count,
                "duplicate_count": metrics.duplicate_tool_calls,
            }),
            weight: 0.30,
        },
        DimensionScore {
            metric: "response_quality".into(),
            score: rq_score,
            source: "llm".into(),
            reason: rq_reason,
            details: json!({}),
            weight: 0.20,
        },
        DimensionScore {
            metric: "performance".into(),
            score: perf_score,
            source: "rule".into(),
            reason: perf_reason,
            details: json!({
                "total_tokens_in": metrics.total_input_tokens,
                "total_tokens_out": metrics.total_output_tokens,
                "avg_duration_ms": metrics.avg_duration_ms,
                "turn_count": metrics.turn_count,
            }),
            weight: 0.15,
        },
    ];

    let overall: f64 = dimensions.iter().map(|d| d.score * d.weight).sum();

    GradeReport {
        session_id: view.session_id.clone(),
        model: view.model.clone(),
        jsonl_ids: view.jsonl_ids.clone(),
        turn_count: view.turns.len(),
        dimensions,
        overall: (overall * 100.0).round() / 100.0,
    }
}

async fn judge_llm(
    view: &SessionView,
    metrics: &types::MetricsSnapshot,
    config: &GraderConfig,
) -> Result<types::JudgeScores, String> {
    let session_text = prompt::format_session(view, metrics);
    let full_prompt = prompt::build_judge_prompt(&session_text);
    judge::judge(config, &full_prompt).await
}
