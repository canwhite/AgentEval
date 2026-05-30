# Probe Module Implementation / 探针模块实现记录

## Overview / 概述

Probe 是一个携带只读文件工具的 LLM agent，进入被评测 agent 的项目目录（`PROBE_SOURCE_PROJECT_DIR`），基于 diagnose 的症状线索审查 prompt / skills / tools / CLAUDE.md 等配置，找到行为问题的根因，并输出结构化改进建议。

**核心原则：只读不写。** 所有改进建议写入 report 的 `recommendation` 字段，由开发者审查后手动执行。

与 grader 共享同一套 LLM 配置（`AGENTEVAL_JUDGE_*`），零新增依赖。

## Files Created / 新增文件

```
src/probe/
├── mod.rs            # run() 入口 + parse_probe_output() + extract_json_block()
├── agent.rs          # AgentLoop — LLM chat → tool dispatch 循环
├── backend.rs        # OpenAiBackend — reqwest → OpenAI-compatible API
├── tool.rs           # Tool trait + Registry（提取自 agnt-core）
├── tools.rs          # ReadFile, Grep, ListDir, Glob — 四个只读工具
├── prompt.rs         # System prompt builder + session summary formatter
├── types.rs          # ProbeReport, ProbeFinding, Message, LlmResponse, SSE/JSON 解析
└── system_prompt.txt # 完整 system prompt（审查方法论 + 配置映射表 + 输出格式）
```

## Files Modified / 修改文件

| File | Change |
|------|--------|
| `src/config.rs` | `ProbeConfig { source_project_dir }` — LLM 配置复用 `GraderConfig` |
| `src/main.rs` | `mod probe;` + CLI `probe` subcommand + API routes |
| `src/proxy.rs` | `AppState` 新增 `probe_config: ProbeConfig` |
| `src/web/mod.rs` | `probe_session()` POST + `get_probe()` GET + `probe_summary` in `list_sessions` |
| `src/web/ui.html` | Dashboard probe column + detail view probe panel + JS functions |

## Module: `src/probe/types.rs`

### Public types — output

```rust
struct ProbeReport {
    session_id: String,           // set by system after parsing
    probed_at: String,            // ISO 8601
    findings: Vec<ProbeFinding>,          // one per diagnose issue
    additional_findings: Vec<ProbeFinding>, // discovered during exploration
    overall_assessment: String,   // summary of all issues
    parse_error: Option<String>,  // raw LLM output if JSON parsing failed
}

struct ProbeFinding {
    issue_title: String,          // matching diagnose issue title
    category: String,             // Skill | SystemPrompt | Tool | CLAUDE.md | General
    root_cause: String,           // what specifically in config causes the behavior
    affected_files: Vec<String>,  // relative paths examined
    confidence: String,           // "high" | "medium" | "low"
    recommendation: String,       // specific, actionable fix
    evidence: String,             // verbatim config excerpt
}

struct ProbeSummary {             // shown in dashboard list
    total_findings: usize,
    high: usize, medium: usize, low: usize,
}
```

### Internal types — agent loop

```rust
struct Message {            // OpenAI-flavored conversation message
    role: String,           // "system" | "user" | "assistant" | "tool"
    content: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
    tool_call_id: Option<String>,
    name: Option<String>,
}
// Constructors: Message::system(), ::user(), ::assistant_with_tool_calls(), ::tool_result()

struct LlmResponse {
    content: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<UsageStats>,
}
```

### Response parsing — `parse_llm_response()`

自动区分非流式（JSON）和流式（SSE）响应：
- JSON: `choices[0].message.content` + `tool_calls[]`
- SSE: 逐行解析 `data:` 块，累积 delta content 和 tool_calls（按 index 分片合并）

## Module: `src/probe/tool.rs`

从 agnt-core 提取，去掉 `TypedTool`/`ErasedAdapter` 抽象层，只保留核心：

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;                  // JSON Schema for OpenAI function calling
    fn call(&self, args: Value) -> Result<String, String>;
}

pub struct Registry { tools: Vec<Box<dyn Tool>> }
// dispatch(name, args) → Result<String, String>
// as_openai_tools() → Value  // → OpenAI function calling JSON array
```

## Module: `src/probe/tools.rs`

四个工具全部只读。路径通过 `resolve_path()` 沙箱化——所有路径相对 `PROBE_SOURCE_PROJECT_DIR` resolve，`..` 直接拒绝，存在路径用 canonicalize 二次校验。

| Tool | 实现 | 限制 | 安全 |
|------|------|------|------|
| `read_file` | `std::fs::read_to_string` | 1MB 截断 + 标记 | resolve_path |
| `grep` | `std::process::Command("grep") -rn --binary-files=without-match` | 1000 行截断 | resolve_path |
| `list_dir` | `std::fs::read_dir` → `TYPE  NAME` 格式 | 无限制 | resolve_path |
| `glob` | `std::process::Command("find") -name -type f`，结果 strip source_dir 前缀 | 2000 条截断 | resolve_path |

## Module: `src/probe/backend.rs`

```rust
pub struct OpenAiBackend {
    client: reqwest::Client,  // timeout: 300s
    api_base: String,
    model: String,
    api_key: String,
}
// chat(messages, tools) → Result<LlmResponse, String>
```

- POST `{api_base}/chat/completions`
- 请求体：`{ model, messages, tools, tool_choice: "auto", stream: false }`
- Bearer token 认证
- 非 2xx 响应返回 body text 作为错误信息

## Module: `src/probe/prompt.rs`

### `build_system_prompt()`

从 `src/probe/system_prompt.txt` 加载（`include_str!`），约 120 行。包含：
- 角色定义 + 只读约束
- 审查方法论（6 步：识别→搜索→分析→交叉验证→发散探究→输出）
- 诊断→配置映射表（10 条 diagnose rule → 重点审查的配置）
- 置信度定义（high/medium/low）
- 四阶段探索策略（概览→定位→深挖→收束）
- 输出 JSON 格式 + 字段说明

### `build_user_prompt(issues, view)`

拼接两部分：
1. JSON 格式的 diagnose issues 列表
2. SessionView 紧凑摘要（`format_session_summary()`）

### `format_session_summary(view, issues)`

- 模型名、turn 总数
- diagnose 标记的 turn 展开细节（user input, tool calls + 结果, 文本回复, token 用量）
- 普通 turn 用 `... (N normal turns omitted) ...` 折叠
- user input 截断到 300 字符，tool call arguments 截断到 100 字符

## Module: `src/probe/agent.rs`

```rust
pub struct AgentLoop {
    backend: OpenAiBackend,
    tools: Registry,
    messages: Vec<Message>,
    max_steps: usize,              // 30
    call_history: HashMap<String, u32>,  // (name|args) → count
}

// async fn run(&mut self) -> Result<String, String>
```

### Loop 流程

```
for step in 0..max_steps:
    response = backend.chat(messages, tools_schema)
    
    if response.has_tool_calls():
        for each tool_call:
            key = "name|arguments"
            call_history[key] += 1
            if count >= 3:
                push warning message → "重复调用了，换策略或输出报告"
            
            result = tools.dispatch(name, args)
            push tool_result message
        
        push assistant message (with tool_calls)
    
    else:
        return response.content  → LLM 输出 JSON report

return Err("max_steps exceeded")
```

### 安全机制

| 机制 | 触发条件 | 行为 |
|------|---------|------|
| max_steps | 30 步 | 返回 error，调用方显示 "probe 超时" |
| Loop detection | 同一 (name, args) 出现 3 次 | 注入 user warning message，促使 LLM 换策略 |
| eprintln 日志 | 每步 | `[probe] step N: calling tool (id=...)` + 结果摘要 |

### 进度日志示例

```
[probe] step 1: calling list_dir (id=call_abc)
[probe]   → list_dir OK (1234 chars)
[probe] step 2: calling read_file (id=call_def)
[probe]   → read_file OK (5678 chars)
    ...
[probe] done — LLM returned final response (no tool calls)
[probe] parsing LLM output (12345 chars)...
[probe] parsed: 3 findings + 1 additional, overall=The main issue...
[probe] writing 8521 bytes to ./logs/session_xxx.probe.json...
[probe] report written successfully.
```

## Module: `src/probe/mod.rs`

### `run(session_id, log_dir, probe_config, grader_config) -> Result<ProbeReport, String>`

1. 读取 `{log_dir}/{session_id}.diagnose.json` → issues（必须已有 diagnose）
2. 读取 `{log_dir}/{session_id}.view.json` → SessionView
3. 验证 `PROBE_SOURCE_PROJECT_DIR` 存在且为目录
4. 构建 system prompt + user prompt
5. 注册四个 file tools，指向 source project dir
6. 创建 `OpenAiBackend`，复用 grader config（`judge_api_base`、`judge_model`、`judge_api_key`）
7. 启动 `AgentLoop`（max_steps=30）
8. `parse_probe_output()` 解析 LLM 输出
9. 写 `{log_dir}/{session_id}.probe.json`
10. 返回 `ProbeReport`

### `read_existing(session_id, log_dir) -> Result<ProbeReport, String>`

读取已有 `.probe.json`，用于 Web API GET。

### `parse_probe_output(session_id, raw) -> ProbeReport`

解析策略（降级链）：
1. `serde_json::from_str(raw)` — 直接 JSON 解析
2. `extract_json_block(raw)` — 提取 markdown 代码块（```json ... ```）
3. 都失败 → 返回带 `parse_error` 的 report，raw content 保留

### `extract_json_block(text) -> Option<&str>`

1. 查找 ` ```json ... ``` `
2. 查找 ` ``` ... ``` `（无语言标记）
3. 查找第一个 `{` 到最后一个 `}`

## API Endpoints / 接口

| Method | Path | Purpose |
|--------|------|---------|
| `POST` | `/dashboard/api/sessions/{id}/probe` | 执行 probe（同步返回，可能耗时） |
| `GET` | `/dashboard/api/sessions/{id}/probe` | 读取已有 `.probe.json`（404 如果未执行） |

Session list (`GET /dashboard/api/sessions`) 中每条 session 现在包含 `probe_summary` 字段：

```json
{
  "probe_summary": {
    "total_findings": 4,
    "high": 2,
    "medium": 1,
    "low": 1
  }
}
```

## CLI / 命令行

```bash
cargo run -- probe <session_id>
```

终端实时显示 tool calls 过程，最终输出 `ProbeReport` JSON。

## Config / 配置

| 环境变量 | 默认值 | 说明 |
|---------|--------|------|
| `PROBE_SOURCE_PROJECT_DIR` | (空) | 被评测 agent 的项目根目录 |
| `AGENTEVAL_JUDGE_API_BASE` | (同 upstream) | Probe LLM API 地址（复用 grader） |
| `AGENTEVAL_JUDGE_MODEL` | `deepseek-chat` | Probe LLM 模型（复用 grader） |
| `AGENTEVAL_JUDGE_API_KEY` | (空) | Probe LLM API key（复用 grader） |

## UI / 界面

### Dashboard — probe badge

- 仅当 session 有 diagnose issues 时显示
- **未执行**: `[Probe]` 按钮 → 点击 POST /probe，更新 cache
- **执行中**: `...` pending 状态
- **有 findings**: `🔍 N findings` badge（点击跳转详情）
- **无 findings**: `✓ no findings`

### Session Detail — probe panel

位于 diagnose panel 下方：
- 已执行：完整 findings 列表（category badge + confidence 颜色 + root_cause + recommendation + evidence）
- 未执行 + 有 diagnose：`[🔍 Run Probe]` 按钮
- 未执行 + 无 diagnose：提示 "Run diagnosis first to enable probe"
- `[🔄 Re-probe]` 按钮支持重新执行

## Persistence / 持久化

```
logs/
  session_xxx.view.json
  session_xxx.grade.json
  session_xxx.diagnose.json
  session_xxx.probe.json    ← NEW
```

## Key Design Decisions / 关键设计决策

1. **零新增依赖** — 全部使用已有 crate（reqwest, serde, tokio, chrono）。Tool trait + Registry 从 agnt 提取而非依赖完整库。

2. **LLM 配置复用 grader** — 不引入 `PROBE_LLM_*` 环境变量，直接用 `AGENTEVAL_JUDGE_*`。probe 和 grader 用同一套 API。

3. **只读工具 + 路径沙箱** — `resolve_path()` 两层防护：字符串 `..` 拒绝 + canonicalize 验证。不提供任何写入/执行工具。

4. **同步 tool 调用** — tool call 逐个执行（非并行），保证 `eprintln!` 日志顺序清晰，便于观察 agent 行为。

5. **reqwest 300s 超时** — 防止大项目场景下 LLM API 调用无限等待。

6. **JSON 解析降级链** — 直接解析 → markdown 代码块提取 → parse_error 兜底。LLM 输出格式不严格时也能拿到结果。

7. **session_id 从最后一个 `_` 分割** — 与 diagnose 模块一致，`session_1779871837_1` → jsonl_stem = `session_1779871837`。

8. **系统常量集中** — system prompt 独立为 `.txt` 文件（`include_str!`），修改 prompt 不需要动 Rust 代码。

## Known Limitations / 已知限制

| Limitation | Detail |
|-----------|--------|
| 依赖目录未过滤 | `grep` 和 `glob` 当前未排除 `node_modules`、`target`、`vendor` 等（plan 已写好，待执行） |
| tool 调用阻塞 runtime | `grep`/`find` 使用 `std::process::Command::output()` 同步阻塞，大项目可能卡几秒 |
| 单 session | CLI 一次只探一个 session，不支持批量 |
| 无行为基线对比 | 只审查配置静态文本，不对比"同模型正常 session 的配置差异" |
