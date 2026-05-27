pub mod types;

use std::collections::HashMap;
use std::io::Write;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::format;
use types::*;

pub async fn run(mut rx: UnboundedReceiver<TurnRecord>, log_dir: String) {
    let mut builder: Option<SessionBuilder> = None;

    while let Some(record) = rx.recv().await {
        // 跳过空 body（连接测试等）
        if record.request_body.is_null() {
            continue;
        }

        if builder.is_none() {
            let model = record
                .request_body
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let session_id = format!("session_{}", record.id);
            builder = Some(SessionBuilder::new(session_id, model));
        }

        let b = builder.as_mut().unwrap();
        b.process(&record.request_body, &record.response_body, record.duration_ms);

        // 每轮完成后覆写 view.json
        let view_path = format!("{}/{}.view.json", log_dir, b.session_id);
        let view = b.build();
        if let Ok(json) = serde_json::to_string_pretty(&view) {
            let mut file = std::fs::File::create(&view_path).unwrap();
            file.write_all(json.as_bytes()).ok();
        }
    }
}

pub struct SessionBuilder {
    session_id: String,
    model: String,
    turns: Vec<Turn>,
    /// 上一轮 request 的 messages，用于 diff
    prev_messages: Vec<Value>,
    /// 待配对的 tool_call: call_id → (turn_index, step_index)
    pending_tool_calls: HashMap<String, (usize, usize)>,
    turn_counter: u64,
}

impl SessionBuilder {
    pub fn new(session_id: String, model: String) -> Self {
        Self {
            session_id,
            model,
            turns: Vec::new(),
            prev_messages: Vec::new(),
            pending_tool_calls: HashMap::new(),
            turn_counter: 0,
        }
    }

    pub fn process(&mut self, request_body: &Value, response_body: &Value, duration_ms: u64) {
        self.turn_counter += 1;

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

        // 4. 跨 turn 配对：看有没有之前 tool_call 的 result 已经到了
        //    本轮 tool_results 中有上一轮 tool_call 的结果
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
        let turn_idx = self.turns.len(); // 即将插入的 turn 的索引
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
            turns: self.turns.clone(),
        }
    }
}

/// 对相邻两轮的 messages 做前缀匹配，返回本轮新增的消息（slice）
fn diff_messages<'a>(prev: &[Value], current: &'a [Value]) -> &'a [Value] {
    let common = prev
        .iter()
        .zip(current.iter())
        .take_while(|(a, b)| a == b)
        .count();
    &current[common..]
}
