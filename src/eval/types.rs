use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 从 proxy 发给 eval 的原始数据
#[derive(Debug, Clone)]
pub struct TurnRecord {
    pub id: u64,
    pub request_body: Value,
    pub response_body: Value,
    pub duration_ms: u64,
}

/// 完整的会话结构化视图
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub session_id: String,
    pub model: String,
    pub upstream: String,
    /// 该 session 包含的 JSONL 行 ID，回溯原始流量
    #[serde(default)]
    pub jsonl_ids: Vec<u64>,
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub turn_id: u64,
    /// 本轮新增的 user 消息文本
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub user_input: Vec<String>,
    /// 本轮提交的 tool 执行结果
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResult>,
    /// 模型产出的步骤序列
    pub steps: Vec<Step>,
    /// token 用量
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// 耗时（毫秒）
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Step {
    #[serde(rename = "reasoning")]
    Reasoning { content: String },
    #[serde(rename = "text")]
    Text { content: String },
    #[serde(rename = "tool_call")]
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
        /// 跨 turn 回填的执行结果，配对上之前为 null
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<ToolResult>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
