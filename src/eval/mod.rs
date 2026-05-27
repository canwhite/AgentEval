pub mod types;

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::config::GraderConfig;
use crate::format;
use crate::grader;
use types::*;

pub async fn run(
    mut rx: UnboundedReceiver<TurnRecord>,
    log_dir: String,
    jsonl_stem: String,
    grader_config: GraderConfig,
) {
    let mut builder: Option<SessionBuilder> = None;
    let mut session_counter: u32 = 0;

    loop {
        // select! 实现闲置超时：收到 record / channel 关闭 / 10 分钟到期
        let record = tokio::select! {
            recv = rx.recv() => recv,
            _ = tokio::time::sleep(Duration::from_secs(120)) => {
                // 超时 → 封口当前 session（view 立即写，grade 后台跑）
                if let Some(ref b) = builder {
                    session_counter += 1;
                    seal_and_grade_bg(b, &log_dir, &jsonl_stem, session_counter, &grader_config);
                    builder = None;
                }
                continue;
            }
        };

        // channel 关闭
        let record = match record {
            Some(r) => r,
            None => break,
        };

        // 跳过空 body（连接测试等）
        if record.request_body.is_null() {
            eprintln!("[eval] id={:04} skipped (null body)", record.id);
            continue;
        }

        // 检测 message 回退 → 新 session
        if let Some(ref b) = builder {
            let current_msgs = format::openai::parse_request_messages(&record.request_body);
            let common = common_prefix_len(&b.prev_messages, &current_msgs);
            eprintln!(
                "[eval] id={:04} prev_msgs={} cur_msgs={} common={}",
                record.id,
                b.prev_messages.len(),
                current_msgs.len(),
                common
            );
            if is_new_session(&b.prev_messages, &current_msgs) {
                eprintln!("[eval] id={:04} -> NEW SESSION detected, sealing + bg grade", record.id);
                session_counter += 1;
                seal_and_grade_bg(b, &log_dir, &jsonl_stem, session_counter, &grader_config);
                builder = None;
            }
        }

        // 创建新 SessionBuilder
        if builder.is_none() {
            let model = extract_model(&record.request_body);
            let session_id = format!("{}_{}", jsonl_stem, session_counter + 1);
            builder = Some(SessionBuilder::new(session_id, model));
        }

        let b = builder.as_mut().unwrap();
        b.process(
            &record.request_body,
            &record.response_body,
            record.duration_ms,
            record.id,
        );
        write_view_json(b, &log_dir);
    }

    // channel 关闭 → 封口最后一个 session（同步等 grader，因为进程即将退出）
    if let Some(ref b) = builder {
        if !b.turns.is_empty() {
            session_counter += 1;
            let final_id = format!("{}_{}", jsonl_stem, session_counter);
            let view = build_with_id(b, &final_id);
            write_view_final(&view, &log_dir);
            eprintln!("[eval] final session {} ({} turns), running grader...", final_id, view.turns.len());
            let report = grader::run_pipeline(&view, &grader_config).await;
            write_grade_json(&report, &log_dir);
            eprintln!("[eval] grade written to {}/{}.grade.json (overall: {})", log_dir, final_id, report.overall);
        }
    }
}

// ── SessionBuilder ──

pub struct SessionBuilder {
    session_id: String,
    model: String,
    turns: Vec<Turn>,
    /// 该 session 包含的 JSONL 行 ID
    jsonl_ids: Vec<u64>,
    pub prev_messages: Vec<Value>,
    pending_tool_calls: HashMap<String, (usize, usize)>,
    turn_counter: u64,
}

impl SessionBuilder {
    pub fn new(session_id: String, model: String) -> Self {
        Self {
            session_id,
            model,
            turns: Vec::new(),
            jsonl_ids: Vec::new(),
            prev_messages: Vec::new(),
            pending_tool_calls: HashMap::new(),
            turn_counter: 0,
        }
    }

    pub fn process(
        &mut self,
        request_body: &Value,
        response_body: &Value,
        duration_ms: u64,
        jsonl_id: u64,
    ) {
        self.turn_counter += 1;
        self.jsonl_ids.push(jsonl_id);

        // 1. 解析 request messages
        let current_messages = format::openai::parse_request_messages(request_body);
        if current_messages.is_empty() {
            return;
        }

        // 2. Diff：找到本轮新增的 user / tool 消息
        let new_messages = diff_messages(&self.prev_messages, &current_messages);
        let mut user_input = Vec::new();
        let mut tool_results = Vec::new();

        for msg in new_messages {
            match msg.get("role").and_then(|v| v.as_str()) {
                Some("user") => {
                    let text = format::openai::extract_text_content(msg);
                    if !text.is_empty() {
                        user_input.push(text);
                    }
                }
                Some("tool") => {
                    let call_id = msg
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let content = format::openai::extract_text_content(msg);
                    tool_results.push(ToolResult {
                        call_id,
                        content,
                        is_error: false,
                    });
                }
                _ => {}
            }
        }

        // 3. 解析 response steps
        let steps = format::openai::parse_response_steps(response_body);

        // 4. 跨 turn 配对 tool_call ↔ tool_result
        for tr in &tool_results {
            if let Some((turn_idx, step_idx)) = self.pending_tool_calls.remove(&tr.call_id) {
                if let Some(turn) = self.turns.get_mut(turn_idx) {
                    if let Some(Step::ToolCall { result, .. }) = turn.steps.get_mut(step_idx) {
                        *result = Some(tr.clone());
                    }
                }
            }
        }

        // 5. 注册本轮新的 tool_call
        let turn_idx = self.turns.len();
        for (i, step) in steps.iter().enumerate() {
            if let Step::ToolCall { call_id, .. } = step {
                if !call_id.is_empty() {
                    self.pending_tool_calls
                        .insert(call_id.clone(), (turn_idx, i));
                }
            }
        }

        // 6. 解析 usage
        let usage = format::openai::parse_response_usage(response_body);

        // 7. 组装 Turn
        let turn = Turn {
            turn_id: self.turn_counter,
            user_input,
            tool_results,
            steps,
            usage,
            duration_ms,
        };

        self.turns.push(turn);
        self.prev_messages = current_messages;
    }

    pub fn build(&self) -> SessionView {
        SessionView {
            session_id: self.session_id.clone(),
            model: self.model.clone(),
            upstream: String::new(),
            jsonl_ids: self.jsonl_ids.clone(),
            turns: self.turns.clone(),
        }
    }
}

// ── 辅助函数 ──

fn diff_messages<'a>(prev: &[Value], current: &'a [Value]) -> &'a [Value] {
    let common = prev
        .iter()
        .zip(current.iter())
        .take_while(|(a, b)| a == b)
        .count();
    &current[common..]
}

fn common_prefix_len(prev: &[Value], current: &[Value]) -> usize {
    prev.iter()
        .zip(current.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

fn is_new_session(prev: &[Value], current: &[Value]) -> bool {
    common_prefix_len(prev, current) <= 1
}

fn extract_model(body: &Value) -> String {
    body.get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string()
}

fn build_with_id(builder: &SessionBuilder, session_id: &str) -> SessionView {
    SessionView {
        session_id: session_id.to_string(),
        model: builder.model.clone(),
        upstream: String::new(),
        jsonl_ids: builder.jsonl_ids.clone(),
        turns: builder.turns.clone(),
    }
}

// ── 文件写入 ──

fn write_view_json(builder: &SessionBuilder, log_dir: &str) {
    let view = builder.build();
    let path = format!("{}/{}.view.json", log_dir, view.session_id);
    write_json_file(&path, &view);
}

fn write_view_final(view: &SessionView, log_dir: &str) {
    let path = format!("{}/{}.view.json", log_dir, view.session_id);
    write_json_file(&path, view);
}

fn write_grade_json(report: &grader::types::GradeReport, log_dir: &str) {
    let path = format!("{}/{}.grade.json", log_dir, report.session_id);
    write_json_file(&path, report);
}

fn write_json_file(path: &str, value: &impl serde::Serialize) {
    if let Ok(json) = serde_json::to_string_pretty(value) {
        if let Ok(mut file) = std::fs::File::create(path) {
            file.write_all(json.as_bytes()).ok();
        }
    }
}

/// 封口当前 session：立即写 view.json，后台 spawn 评分任务（不阻塞主循环）
fn seal_and_grade_bg(
    builder: &SessionBuilder,
    log_dir: &str,
    jsonl_stem: &str,
    counter: u32,
    grader_config: &GraderConfig,
) {
    let final_id = format!("{}_{}", jsonl_stem, counter);
    let view = build_with_id(builder, &final_id);
    write_view_final(&view, log_dir);

    eprintln!("[eval] session {} sealed ({} turns), grading in background...", final_id, view.turns.len());

    let grader_config = grader_config.clone();
    let log_dir = log_dir.to_string();
    tokio::spawn(async move {
        let report = grader::run_pipeline(&view, &grader_config).await;
        write_grade_json(&report, &log_dir);
        eprintln!("[eval] grade written to {}/{}.grade.json (overall: {})", log_dir, final_id, report.overall);
    });
}
