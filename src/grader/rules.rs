use crate::eval::types::{SessionView, Step};
use super::types::MetricsSnapshot;

pub fn extract_metrics(view: &SessionView) -> MetricsSnapshot {
    let mut total_calls = 0usize;
    let mut error_count = 0usize;
    let mut duplicate_count = 0usize;
    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut total_duration: u64 = 0;
    let mut has_tool_error = false;
    let mut final_text_len: usize = 0;
    let mut has_final_text = false;

    // 检测重复 tool_call：同名同参出现 ≥2 次
    let mut call_signatures: Vec<String> = Vec::new();

    for turn in &view.turns {
        total_duration += turn.duration_ms;

        if let Some(ref usage) = turn.usage {
            total_input += usage.input_tokens;
            total_output += usage.output_tokens;
        }

        for step in &turn.steps {
            match step {
                Step::Reasoning { .. } => {}
                Step::Text { content } => {
                    // 累积每轮文本长度（最后一轮会覆盖）
                    if turn.turn_id == view.turns.len() as u64 {
                        final_text_len += content.len();
                        has_final_text = true;
                    }
                }
                Step::ToolCall { name, arguments, result, .. } => {
                    total_calls += 1;
                    let sig = format!("{}:{}", name, arguments);
                    if call_signatures.contains(&sig) {
                        duplicate_count += 1;
                    } else {
                        call_signatures.push(sig);
                    }
                    if let Some(r) = result {
                        if r.is_error {
                            error_count += 1;
                            has_tool_error = true;
                        }
                        // 也检查内容是否像 error
                        if r.content.starts_with("Error:") || r.content.starts_with("error:") {
                            has_tool_error = true;
                        }
                    }
                }
            }
        }
    }

    let turn_count = view.turns.len();
    let avg_duration = if turn_count > 0 {
        total_duration / turn_count as u64
    } else {
        0
    };

    MetricsSnapshot {
        total_tool_calls: total_calls,
        tool_success_count: total_calls.saturating_sub(error_count),
        tool_error_count: error_count,
        duplicate_tool_calls: duplicate_count,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        avg_duration_ms: avg_duration,
        turn_count,
        final_text_len,
        has_final_text,
        has_tool_error,
    }
}

/// 工具效率评分（0.0~1.0）
pub fn calc_tool_efficiency(m: &MetricsSnapshot) -> (f64, String) {
    if m.total_tool_calls == 0 {
        return (1.0, "无工具调用".to_string());
    }

    let success_rate = m.tool_success_count as f64 / m.total_tool_calls as f64;
    // 重复惩罚：每个重复扣 0.1
    let dup_penalty = (m.duplicate_tool_calls as f64 * 0.1).min(0.5);
    let score = (success_rate - dup_penalty).max(0.0).min(1.0);

    let reason = if m.tool_error_count > 0 {
        format!(
            "{} 次调用，{} 次失败，{} 次重复",
            m.total_tool_calls, m.tool_error_count, m.duplicate_tool_calls
        )
    } else if m.duplicate_tool_calls > 0 {
        format!(
            "{} 次调用全部成功，但有 {} 次重复",
            m.total_tool_calls, m.duplicate_tool_calls
        )
    } else {
        format!("{} 次调用全部成功，无重复", m.total_tool_calls)
    };

    (score, reason)
}

/// 性能评分（0.0~1.0）
pub fn calc_performance(m: &MetricsSnapshot) -> (f64, String) {
    if m.turn_count == 0 {
        return (1.0, "无数据".to_string());
    }

    // token 效率：output/input 在 0.01~0.3 为理想区间
    let token_ratio = if m.total_input_tokens > 0 {
        m.total_output_tokens as f64 / m.total_input_tokens as f64
    } else {
        0.0
    };
    let token_score = if token_ratio < 0.01 {
        0.3 // output 太少
    } else if token_ratio > 0.5 {
        0.5 // output 太多，可能浪费
    } else {
        // 0.05~0.3 最佳
        1.0 - ((token_ratio - 0.15).abs() * 2.0).min(0.7)
    };

    // 耗时：单轮平均 < 30s 满分
    let latency_score = if m.avg_duration_ms < 10_000 {
        1.0
    } else if m.avg_duration_ms < 30_000 {
        0.8
    } else {
        0.5
    };

    // turn 数量：排除纯 tool 轮次后，3~15 为理想
    let turn_score = match m.turn_count {
        0..=2 => 0.7,   // 太少，可能没充分交互
        3..=15 => 1.0,
        _ => 0.6,       // 太多
    };

    let score = (token_score * 0.3 + latency_score * 0.4 + turn_score * 0.3).min(1.0);

    let reason = format!(
        "{} tokens in / {} tokens out, avg {}ms, {} turns",
        m.total_input_tokens, m.total_output_tokens, m.avg_duration_ms, m.turn_count
    );

    (score, reason)
}
