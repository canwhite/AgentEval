# Agent 评测状态机方案

## 目标

从原始 API 请求/响应中重建完整的 agent 会话结构化视图，识别：
- Turn 边界（user → model → tools → model → ...）
- Tool 调用与结果配对
- 每步的 token 消耗和耗时

## 核心思路

**不建显式状态机，直接解析 API 原生 payload。** 每次 API 调用的 request body 本身携带了完整 conversation history，只需做 diff + parse。

```
原始 JSONL（每行 = 一次 API 调用）
        │
        ▼  format parser
  ┌─────────────────┐
  │ 解析 request body │ → 提取 messages[]（完整会话历史）
  │ 解析 response body│ → 提取 steps（text / tool_call / reasoning）
  └─────────────────┘
        │
        ▼  session builder
  ┌─────────────────┐
  │ diff 相邻轮次     │ → messages 前缀匹配，新增 = user_input 或 tool_result
  │ 配对 tool 调用    │ → HashMap<call_id, (turn_idx, step_idx)> 跨轮回填
  │ 组装 SessionView  │
  └─────────────────┘
        │
        ▼
  session_xxx.view.json   ← 结构化评测数据，每轮增量覆写
```

## 数据流

```
proxy.rs（已有）
  │  转发请求到上游，流式响应返回给 agent
  │  后台任务：累积完整 response body，写 JSONL 行
  │  新增：写完 JSONL 后发送 TurnRecord 到 eval channel
  │
  ▼  mpsc::unbounded_channel
  │
eval/mod.rs（新增）
  │  format::parse_request_messages()    → Vec<Value>
  │  format::parse_response_steps()      → Vec<Step>
  │  SessionBuilder.process()            → 追加 Turn
  │  writer 增量覆写 session_xxx.view.json
```

## 数据结构

```
SessionView
  ├── session_id
  ├── model, upstream
  └── turns[]
        ├── turn_id
        ├── user_input[]          ← 本轮新增的 user 消息
        ├── tool_results[]        ← 本轮提交的 tool 执行结果
        ├── steps[]               ← 模型产出的步骤
        │     ├── Reasoning { content }
        │     ├── Text { content }
        │     └── ToolCall { call_id, name, arguments, result? }
        ├── usage { input_tokens, output_tokens }
        └── duration_ms
```

## 解析逻辑

### 1. 从 request body 提取新消息（diff）

对相邻两轮做 messages 前缀匹配：

```
Turn N-1 request: [sys, user("A"), assistant("..."), tool("r1")]
Turn N   request: [sys, user("A"), assistant("..."), tool("r1"), user("追问")]
                  └─ 相同前缀 ────────────────────────────────┘ └─ 新增 ─┘
```

新增消息按 role 分类：`user` → user_input，`tool` → tool_result。

### 2. 从 response body 提取 model 步骤

- **非流式**：直接解析 `choices[0].message.content` 和 `.tool_calls[]`
- **流式（SSE）**：逐行解析 `data:` 行，累积 content / reasoning delta，按 index 重组 tool_calls 的 function.arguments

### 3. 跨 turn 配对 tool_use ↔ tool_result

- HashMap<call_id, (turn_index, step_index)>
- 遇到 ToolCall → 注册到 map，result 置 None
- 下轮遇到 ToolResult → 按 call_id 查找，回填 result

首轮 N 的 tool_call 在轮 N+1 的 tool_results 中找到匹配。

### 4. Usage 提取

response body 的 `usage` 字段：
```
usage: { prompt_tokens, completion_tokens, total_tokens }
```

## 文件结构

```
src/
  format/
    mod.rs          # detect_format()，委托到具体 parser
    openai.rs       # parse_request_messages(), parse_response_steps()
  eval/
    mod.rs          # SessionBuilder（diff + 配对 + 增量写）
    types.rs        # SessionView, Turn, Step, TurnRecord
  proxy.rs          # +eval_tx channel，发送 TurnRecord
  main.rs           # +spawn eval 消费者
```

## 集成改动

### proxy.rs
- AppState 新增 `eval_tx: UnboundedSender<TurnRecord>`
- 后台任务写 JSONL 后 `eval_tx.send(TurnRecord { ... }).ok()`

### main.rs
- 创建 `mpsc::unbounded_channel::<TurnRecord>()`
- 构造 AppState 时传入 sender
- spawn `eval::run(rx, log_dir)` 消费者

## 输出

`logs/session_xxx.view.json` 和 `logs/session_xxx.jsonl` 并存：
- `.jsonl` — 原始 API 流量，每行一次 API 调用
- `.view.json` — 结构化会话视图，每轮完成后增量覆写
