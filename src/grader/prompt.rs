use crate::eval::types::{SessionView, Step};
use super::types::MetricsSnapshot;

/// 把 SessionView 格式化成 LLM 易读的会话摘要
pub fn format_session(view: &SessionView, metrics: &MetricsSnapshot) -> String {
    let mut buf = String::new();

    buf.push_str("## 会话概览\n");
    buf.push_str(&format!("- 模型: {}\n", view.model));
    buf.push_str(&format!("- Turn 数: {}\n", metrics.turn_count));
    buf.push_str(&format!(
        "- Token: {} in / {} out\n",
        metrics.total_input_tokens, metrics.total_output_tokens
    ));
    buf.push_str(&format!("- 工具调用: {} 次\n", metrics.total_tool_calls));
    buf.push_str(&format!("- 平均耗时: {}ms\n\n", metrics.avg_duration_ms));

    buf.push_str("## 对话记录\n");

    for turn in &view.turns {
        // 用户输入
        for u in &turn.user_input {
            buf.push_str(&format!("用户: \"{}\"\n", truncate(u, 500)));
        }

        // 步骤
        for step in &turn.steps {
            match step {
                Step::Reasoning { content } => {
                    buf.push_str(&format!("  [思考] {}\n", truncate(content, 300)));
                }
                Step::ToolCall { name, arguments, result, .. } => {
                    buf.push_str(&format!(
                        "  [工具调用] {}({})\n",
                        name,
                        truncate(&arguments.to_string(), 200)
                    ));
                    if let Some(r) = result {
                        let label = if r.is_error { "错误" } else { "结果" };
                        buf.push_str(&format!(
                            "  [工具{}] {}\n",
                            label,
                            truncate(&r.content, 500)
                        ));
                    }
                }
                Step::Text { content } => {
                    buf.push_str(&format!("  [回复] {}\n", truncate(content, 1000)));
                }
            }
        }

        // token 和耗时
        if let Some(ref usage) = turn.usage {
            buf.push_str(&format!(
                "  ({} tokens in, {} tokens out, {}ms)\n",
                usage.input_tokens, usage.output_tokens, turn.duration_ms
            ));
        }
        buf.push('\n');
    }

    buf
}

/// 构造完整的 LLM 评审 prompt
pub fn build_judge_prompt(session_text: &str) -> String {
    format!(
        r#"你是一个 agent 评测专家。根据以下 agent 会话记录，对两个维度打分（0.0-1.0）。

{}

## 评分维度

### task_completion（任务完成度）
- 用户最后的问题是否得到了回答？
- agent 是否完成了用户要求的操作（读文件/写文件/搜索等）？
- 如果 agent 只调了工具但没给出文本总结，扣分
- 如果出现 tool error 且未重试成功，扣分
- 如果没有 tool call 也没有实质回复，低分

### response_quality（回复质量）
- 回复是否准确回应了用户的问题？有没有答非所问？
- 回复是否简洁清晰？有没有冗长废话？
- 是否包含用户需要的关键信息？
- 如果回复只是"已读取"/"已完成"等敷衍话，低分

## 输出格式（严格 JSON，不要输出其他内容）
{{
  "task_completion": {{ "score": 0.0-1.0, "reason": "评分理由，中文，一句话" }},
  "response_quality": {{ "score": 0.0-1.0, "reason": "评分理由，中文，一句话" }}
}}"#,
        session_text
    )
}

fn truncate(s: &str, max_len: usize) -> String {
    let s = s.trim();
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}
