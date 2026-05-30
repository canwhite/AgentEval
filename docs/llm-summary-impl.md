# LLM Summary for Diagnose + Probe / 诊断与探针的 LLM 总结

## Overview / 概述

为 diagnose 报告和 probe 报告各自增加一个 LLM 生成的自然语言总结，展示在详情页对应 section 的顶部，让用户一眼看到核心问题。

- **Diagnose**: 纯规则引擎原本不调 LLM。现在 diagnose 完成后会调用 judge LLM 对 issues 列表做 2-3 句总结，存入 `llm_summary` 字段。
- **Probe**: probe agent 原本就输出 `overall_assessment` 字段（LLM 生成），只是之前放在面板底部，现移到顶部。

**容错设计**: LLM 调用是 best-effort — API key 未配置或调用失败时只打 log，不影响 diagnose/probe 主流程。

## Files Modified / 修改文件

| File | Change |
|------|--------|
| `src/diagnose/types.rs` | `DiagnoseReport` 新增 `llm_summary: Option<String>` |
| `src/diagnose/mod.rs` | `run()` 改为 async + 接受 `&GraderConfig` + 新增 `summarize_issues()` |
| `src/main.rs` | CLI `run_diagnose_cli()` 改为 async + 加载 GraderConfig + terminal 展示 summary |
| `src/web/mod.rs` | `diagnose_session` handler 传入 `&state.grader_config` + `.await` |
| `src/web/ui.html` | diagnose panel 顶部展示 `llm_summary`；probe panel 的 `overall_assessment` 从底部移到顶部 |

## Module: `src/diagnose/types.rs`

### `DiagnoseReport` 新增字段

```rust
pub struct DiagnoseReport {
    pub session_id: String,
    pub diagnosed_at: String,
    pub summary: DiagnoseSummary,
    pub issues: Vec<DiagnoseIssue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_summary: Option<String>,  // NEW
}
```

`#[serde(default)]` 保证旧 `.diagnose.json`（不含此字段）反序列化时自动设为 `None`，向后兼容。

## Module: `src/diagnose/mod.rs`

### `run()` 签名变更

```rust
// Before
pub fn run(session_id: &str, log_dir: &str) -> Result<DiagnoseReport, String>

// After
pub async fn run(session_id: &str, log_dir: &str, grader_config: &GraderConfig) -> Result<DiagnoseReport, String>
```

### 流程变更

```
1. 读取 view.json
2. 读取 jsonl entries
3. 运行 10 条诊断规则 → issues
4. summarize_issues(grader_config, &issues).await  → llm_summary  ← NEW
5. 构建 DiagnoseReport（含 llm_summary）
6. 写入 .diagnose.json
```

### `summarize_issues()` 实现

```rust
async fn summarize_issues(config: &GraderConfig, issues: &[DiagnoseIssue]) -> Option<String>
```

**容错策略（降级链）**：

| 条件 | 行为 |
|------|------|
| `judge_api_key` 为空 | 直接返回 `None` |
| `issues` 为空 | 直接返回 `None` |
| HTTP 请求失败 | `eprintln!` log，返回 `None` |
| API 返回非 2xx | `eprintln!` body log，返回 `None` |
| JSON 解析失败 | `eprintln!` log，返回 `None` |

**LLM 调用参数**：

| 参数 | 值 | 说明 |
|------|-----|------|
| URL | `{judge_api_base}/chat/completions` | 与 probe 一致（无 `/v1` 前缀） |
| model | `judge_model`（默认 MiniMax-M2.5） | 复用 grader 配置 |
| temperature | 0.3 | 低温度保证事实准确性 |
| max_tokens | 200 | 2-3 句约 100-150 tokens |
| timeout | 60s | 单次调用无需更长 |

**Prompt 结构**：
```
You are an agent evaluation expert. Below are issues found by automated
diagnosis of an AI agent session. Write a 2-3 sentence summary of the key
problems found and their likely impact on the session quality. Be concise.

Issues:
- [Error] Tool X repeated: agent called tool X 5 times with identical args...
- [Warn] Token usage high: turn 3 exceeded 8000 input tokens...
```

### `read_existing()` 保持不变

只读磁盘，不调 LLM。旧文件反序列化时 `llm_summary` 自动为 `None`。

## CLI / 命令行

### `run_diagnose_cli()` 改为 async

```rust
async fn run_diagnose_cli(args: &[String]) {
    // ...
    let grader_config = config::GraderConfig::load("https://api.deepseek.com");
    match diagnose::run(session_id, &log_dir, &grader_config).await {
        // ...
    }
}
```

Terminal 输出格式在 issues 列表末尾展示 `llm_summary`：
```
--- LLM Summary ---
The session exhibited repeated redundant tool calls and high token
consumption in later turns, suggesting prompt or skill instructions
are insufficient to guide the agent toward efficient behavior.
```

`main()` 中改为 `.await` 调用：
```rust
run_diagnose_cli(&args).await;
```

## Web Handler / 接口

`POST /dashboard/api/sessions/{id}/diagnose` handler 变更：

```rust
let report = diagnose::run(&session_id, &state.log_dir, &state.grader_config)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
```

## UI / 界面

### Diagnose Panel — AI Summary 高亮框

位于 diagnose card 顶部（issue 统计之前），蓝色左边框：

```javascript
if (report.llm_summary) {
    html += '<div style="...background:rgba(68,138,255,0.08);border-left:3px solid var(--blue)...">';
    html += '<b style="color:var(--blue)">AI Summary:</b> ' + escHtml(report.llm_summary);
    html += '</div>';
}
```

### Probe Panel — overall_assessment 移到顶部

`renderProbePanel()` 中 `overall_assessment` 从面板底部（findings 列表之后）移到顶部（findings 统计之前），使用与 diagnose 相同的蓝色左边框样式。

## Key Design Decisions / 关键设计决策

1. **Best-effort 容错** — LLM 调用失败不阻止 diagnose。`summarize_issues()` 返回 `Option<String>`，所有错误路径返回 `None` 并打 log。

2. **LLM 配置复用 grader** — 不引入新环境变量，直接用 `GraderConfig`。与 probe 模式一致。

3. **URL 格式与 probe 一致** — `{base}/chat/completions`（非 grader 的 `/v1/chat/completions`），因为 probe 的格式已验证可用。

4. **diagnose::run() 改为 async** — 因为 `reqwest` 没有启用 `blocking` feature。Cargo.toml 中只有 `["stream", "json"]`。

5. **read_existing 保持同步** — 读取已有 `.diagnose.json` 不需要 LLM 调用，保持简单。

6. **后端兼容** — `#[serde(default)]` 保证新代码可以读取旧格式的 `.diagnose.json`。
