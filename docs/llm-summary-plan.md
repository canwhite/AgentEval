# Plan: LLM Summary for Diagnose + Probe Panels

## Context

用户在 diagnose 和 probe 详情页想看一个 LLM 生成的自然语言总结，放在对应 section 顶部，一眼就能了解核心问题。

- **Probe**: `overall_assessment` 字段已经由 probe agent 的 LLM 生成，只需 UI 从底部移到顶部
- **Diagnose**: 纯规则引擎，没有 LLM 参与。需新增 `llm_summary` 字段 + 调 LLM 生成

## Changes

### 1. `src/diagnose/types.rs` — 加 `llm_summary` 字段

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

`#[serde(default)]` 保证旧 `.diagnose.json` 反序列化兼容。

### 2. `src/diagnose/mod.rs` — async run + summarize 函数

**`diagnose::run()` 改为 async**，接受 `&GraderConfig`：

```rust
pub async fn run(session_id: &str, log_dir: &str, grader_config: &GraderConfig) -> Result<DiagnoseReport, String>
```

新增 `summarize_issues()` — 用 reqwest 调 LLM，URL 格式与 probe 一致 (`{base}/chat/completions`)：
- prompt: 列出 issues 的 severity + title + detail，要求 2-3 句总结
- temperature 0.3, max_tokens 200, timeout 60s
- API key 为空或 issues 为空时直接返回 `None`
- LLM 调用失败只打 log，不 fail 整个 diagnose

`read_existing()` 保持同步不变（只读磁盘）。

### 3. `src/main.rs` — CLI 适配

- `run_diagnose_cli()` 改为 `async fn`
- 内部加载 `GraderConfig::load("https://api.deepseek.com")`
- `diagnose::run(...).await`
- terminal 输出格式展示 `llm_summary`（如果存在）
- `main()` 中 `run_diagnose_cli(&args).await`

### 4. `src/web/mod.rs` — handler 传参

`diagnose_session` 中：
```rust
let report = diagnose::run(&session_id, &state.log_dir, &state.grader_config).await
```

### 5. `src/web/ui.html` — UI 展示

**Diagnose panel** (`renderDiagnosePanel`): 在 card 顶部（issue 统计之前）显示 `llm_summary`，蓝色左边框高亮。

**Probe panel** (`renderProbePanel`): `overall_assessment` 从底部移到 card 顶部（findings 列表之前）。

## Verification

1. `cargo build` 0 warnings
2. `cargo test` 全通过
3. 启动 dashboard，对已有 session 点 Diagnose → 面板顶部出现 LLM 总结
4. 对已有 session 点 Probe → `overall_assessment` 出现在面板顶部
5. CLI `cargo run -- diagnose <session_id> --format terminal` 正常显示总结
6. 无 API key 时 diagnose 正常完成（`llm_summary` 为空，面板不显示）
