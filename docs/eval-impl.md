# Eval 状态机实现总结

## 概述

实现了 agent 评测的 session 重构引擎。从原始 API 流量（JSONL）中重建结构化会话视图（view.json），识别 turn 边界、tool 调用配对、token 消耗和耗时。

## 核心原则

**不建显式状态机，直接解析 API 原生 payload。** 每次 API 调用的 request body 携带完整 conversation history，通过 diff + parse 重建结构。

## 模块划分

```
src/
  format/
    mod.rs          # 模块声明，预留多格式扩展
    openai.rs       # OpenAI Chat Completions 解析（request + 非流式 + SSE）
  eval/
    mod.rs          # SessionBuilder：diff、配对、增量写
    types.rs        # 数据结构定义
  proxy.rs          # 代理层（改动：发送 TurnRecord）
  main.rs           # 入口（改动：spawn eval 消费者）
```

---

## 各模块详情

### 1. `src/eval/types.rs` — 数据结构

```rust
// 从 proxy 发给 eval 的原始数据
pub struct TurnRecord {
    pub id: u64,
    pub request_body: Value,
    pub response_body: Value,
    pub duration_ms: u64,
}

// 完整会话视图
pub struct SessionView {
    pub session_id: String,
    pub model: String,
    pub upstream: String,
    pub turns: Vec<Turn>,
}

pub struct Turn {
    pub turn_id: u64,
    pub user_input: Vec<String>,        // 本轮新增用户消息
    pub tool_results: Vec<ToolResult>,  // 本轮提交的工具结果
    pub steps: Vec<Step>,               // 模型产出序列
    pub usage: Option<Usage>,           // token 消耗
    pub duration_ms: u64,
}

pub enum Step {
    Reasoning { content: String },
    Text { content: String },
    ToolCall { call_id, name, arguments, result: Option<ToolResult> },
}

pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}
```

Serde tag 序列化：`Step` 用 `#[serde(tag = "type")]` 输出 `"type": "text"` / `"type": "tool_call"` / `"type": "reasoning"`。

### 2. `src/format/openai.rs` — OpenAI 协议解析

**职责**：从原始 request/response JSON 中提取结构化数据。

| 函数 | 输入 | 输出 | 逻辑 |
|---|---|---|---|
| `parse_request_messages` | request body | `Vec<Value>` | 提取 `.messages[]` 数组 |
| `parse_response_steps` | response body | `Vec<Step>` | 自动分流非流式/SSE |
| `parse_response_usage` | response body | `Option<Usage>` | 提取 `usage.prompt_tokens` + `completion_tokens` |
| `extract_text_content` | message value | `String` | 处理 content 为 string / array / null |

**非流式解析**（`parse_non_streaming`）：
```
choices[0].message
  ├── reasoning_content? → Step::Reasoning
  ├── tool_calls[]       → Step::ToolCall（arguments 自动 from_str 解析）
  └── content?           → Step::Text
```

**流式 SSE 解析**（`parse_sse`）：
```
逐行解析 "data: " 前缀
  ├── [DONE] → 跳过
  ├── delta.content → 累积到 content_buf
  ├── delta.reasoning_content → 累积到 reasoning_buf
  └── delta.tool_calls[]
        ├── index → 分桶
        ├── id / function.name → 写对应桶
        └── function.arguments → 追加到对应桶
最终按 reasoning → tool_calls → text 顺序输出
```

### 3. `src/eval/mod.rs` — SessionBuilder

**职责**：增量构建会话视图，核心算法。

```rust
pub struct SessionBuilder {
    session_id: String,
    model: String,
    turns: Vec<Turn>,
    prev_messages: Vec<Value>,                             // 上轮 request 的 messages
    pending_tool_calls: HashMap<String, (usize, usize)>,   // call_id → (turn_idx, step_idx)
    turn_counter: u64,
}
```

**`process()` 流程**：

```
1. 解析 request messages
2. Diff 新消息 ────────► 新增的 user → user_input
                         新增的 tool → tool_results
3. 解析 response steps
4. 跨 turn 配对 ─────── 遍历 tool_results，在 pending_tool_calls 中回填
5. 注册新 tool_call ─── 写入 pending_tool_calls
6. 组装 Turn
7. 保存 prev_messages
```

**Diff 算法**（`diff_messages`）：
```rust
fn diff_messages<'a>(prev: &[Value], current: &'a [Value]) -> &'a [Value] {
    let common = prev.iter().zip(current.iter())
        .take_while(|(a, b)| a == b)
        .count();
    &current[common..]
}
```

**Tool 配对**：
```
Turn N:   model 产出 tool_call(call_abc) → 注册到 HashMap
Turn N+1: request messages 中有 tool(call_abc, result)
          → 从 HashMap 查找 → 回填 Step::ToolCall.result
```

**增量写**：每轮 `process()` 后，`build()` 克隆当前状态并覆写 `session_xxx.view.json`。

### 4. `src/proxy.rs` — 改动点

| 改动 | 说明 |
|---|---|
| AppState 新增 `eval_tx` | `UnboundedSender<TurnRecord>` |
| AppState::new 加参数 | 接收 eval sender |
| 后台任务末尾 | 克隆 req/resp body 后 `.send(TurnRecord{...}).ok()` |

改动量：约 15 行，对原有代理逻辑无侵入。

### 5. `src/main.rs` — 改动点

```rust
let (eval_tx, eval_rx) = tokio::sync::mpsc::unbounded_channel::<TurnRecord>();
let state = Arc::new(AppState::new(&config, eval_tx));
tokio::spawn(eval::run(eval_rx, log_dir));
```

新增两个 `mod` 声明：`eval`、`format`。

---

## 数据流全景

```
agent 发请求 ──► proxy:57633 ──► upstream (edgefn)
                    │
                    │ tee response body
                    ▼
              后台任务：累积 resp_buf
                    │
                    ├─► 写 session_xxx.jsonl（原始行）
                    │
                    └─► eval_tx.send(TurnRecord)
                           │
                           ▼  eval::run()
                      SessionBuilder.process()
                           │
                           ├─ format::parse_request_messages()
                           ├─ diff_messages()
                           ├─ format::parse_response_steps()
                           ├─ 跨 turn 配对 tool_call ↔ tool_result
                           │
                           └─► 覆写 session_xxx.view.json
```

## 输出文件

| 文件 | 内容 |
|---|---|
| `logs/session_1768000000.jsonl` | 原始 API 流量，每行一次调用 |
| `logs/session_1768000000.view.json` | 结构化会话视图 |

### view.json 示例

```json
{
  "session_id": "session_2",
  "model": "MiniMax-M2.5",
  "upstream": "",
  "turns": [
    {
      "turn_id": 1,
      "user_input": ["读一下 README.md"],
      "steps": [
        { "type": "tool_call", "call_id": "call_abc", "name": "read", "arguments": { "path": "README.md" } },
        { "type": "text", "content": "README 的内容是..." }
      ],
      "usage": { "input_tokens": 5200, "output_tokens": 300 },
      "duration_ms": 2500
    }
  ]
}
```

## 边界处理

| 场景 | 处理方式 |
|---|---|
| 首轮（无 prev_messages） | diff 返回全部 messages，按 role 过滤 |
| 同轮多个 tool_call | 按数组顺序，各自注册到 HashMap |
| tool call 和 result 跨多轮 | HashMap 持久存在，任意跨度都能配对 |
| 流式响应 body 无法 JSON 解析 | `body_to_json` 降级为 `Value::String(SSE文本)` |
| request body 为空 | eval 跳过（`is_null` 检查） |
| response 无 usage 字段 | 返回 None，不 panic |
