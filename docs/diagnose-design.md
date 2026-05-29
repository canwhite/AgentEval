# Diagnose Module Design / 诊断模块设计

## Overview / 概述

Diagnose（嗅探诊断）模块用于自动检测 Session 中 **Agent 的行为问题**，帮助开发者快速定位 Agent 在工具使用、Prompt 设计、Token 消耗等方面的不合理模式。

诊断是**纯规则引擎**，不调 LLM。以单个 view.json 为主要分析对象，穿透到原始 JSONL 获取补充上下文。目标是回答：**这个 Session 评分低，Agent 哪里做得不好？**

## Architecture / 架构

```
                 ┌──────────────────────┐
Dashboard <0.6   │  [Diagnose] button   │
  entry point    │                      │
                 │  POST /api/diagnose  │
                 └──────────┬───────────┘
                            │
        ┌───────────────────┼───────────────────┐
        │                   │                   │
        ▼                   ▼                   ▼
  Web API             diagnose::run()      CLI entry
  (server process)    (shared logic)       (cargo run -- diagnose <id>)
                            │
                   ┌────────┴────────┐
                   │  read .view.json │
                   │  read .jsonl     │
                   │  run rules       │
                   │  write .diagnose │
                   └─────────────────┘
```

Web API 和 CLI 共享同一套 `diagnose::run()` 逻辑。Web 返回 JSON 给浏览器渲染，CLI 输出到终端。

## Module Structure / 模块结构

```
src/diagnose/
  mod.rs       # run() 主入口 + 文件读写
  types.rs     # DiagnoseReport, DiagnoseIssue, IssueCategory, Severity
  rules.rs     # 10 条诊断规则
```

## Session ID → JSONL File Resolution / 从 session_id 定位 JSONL

Session ID 格式: `{jsonl_stem}_{counter}`，例 `session_1779871837_1`。

**关键:** stem 本身含下划线（`session_` 前缀），需要从**最后一个** `_` 处分割：

```
session_1779871837_1  →  stem: session_1779871837  /  counter: 1
session_1779871837_42 →  stem: session_1779871837  /  counter: 42
```

JSONL 文件路径: `{log_dir}/{jsonl_stem}.jsonl`

对应行的筛选: JSONL 中 `id` 字段 = `session_view.jsonl_ids[]` 中的值。

## Data Types / 数据类型

```rust
// Must derive Serialize + Deserialize for JSON file I/O.
// 必须实现 Serialize + Deserialize 以支持 JSON 文件读写。
struct DiagnoseReport {
    session_id: String,          // e.g. "session_1779871837_1", also the file stem
    diagnosed_at: String,        // ISO timestamp
    summary: DiagnoseSummary,
    issues: Vec<DiagnoseIssue>,
}

struct DiagnoseSummary {
    total_issues: usize,
    errors: usize,
    warnings: usize,
    infos: usize,
}

struct DiagnoseIssue {
    category: IssueCategory,   // Tool | Prompt | Token | View
    severity: Severity,        // Error | Warn | Info
    title: String,             // rule name, e.g. "tool_result_missing"
    detail: String,            // human-readable description
    location: IssueLocation,
    evidence: String,          // relevant raw data excerpt
}

struct IssueLocation {
    jsonl_id: Option<u64>,
    turn_id: Option<u64>,
    step_index: Option<usize>,
}

enum IssueCategory { Tool, Prompt, Token, View }
enum Severity { Error, Warn, Info }
```

## Diagnostic Rules / 诊断规则

### Tool / 工具类

| Rule | Severity | Threshold | Logic |
|------|----------|-----------|-------|
| **tool_result_missing** | Error | — | tool_call.result is None (not backfilled cross-turn) |
| **tool_result_error** | Error | — | tool_result.is_error == true |
| **tool_duplicate_3plus** | Warn | ≥ 3 times | Same name + same arguments called ≥ 3 times (arguments compared structurally: parse JSON, compare key-sorted) |
| **tool_result_empty** | Warn | — | tool_result.content is empty or whitespace-only (tool ran but produced no output). Note: some tools (write, delete) legitimately return empty on success — this rule may produce false positives for those cases. |

### Prompt / 提示词类

| Rule | Severity | Threshold | Logic |
|------|----------|-----------|-------|
| **prompt_bloat** | Warn | > 3000 chars | System prompt content exceeds threshold. Data source: first JSONL entry for this session → `request_body.messages[]` → message with `role: "system"` → content length. |
| **prompt_context_overflow** | Error | — | History truncation detected via JSONL request body: a message with `role: "tool"` whose `tool_call_id` has no matching `tool_calls[].id` in preceding assistant messages within the same request — the tool call that produced this result was truncated from history. |

### Token / Token 消耗类

All token rules operate on **per-JSONL-entry** (single API request) level, not on aggregated Turn-level usage.

*所有 token 规则基于**单条 JSONL 记录**（单次 API 请求），不使用 Turn 级别的聚合 usage。*

| Rule | Severity | Threshold | Logic |
|------|----------|-----------|-------|
| **token_empty_response** | Error | — | response_body.choices empty or content empty |
| **token_waste** | Warn | per-request output/input < 2% & per-request input > 5000 | Massive input, negligible output. Note: hard threshold at 5000 input tokens / 2% ratio creates a cliff edge — 5001 in + 100 out (2.00%) passes, 5001 in + 99 out (1.98%) flags. This is intentional: the exact boundary anchors on "did the agent accomplish anything proportional to context spent." |
| **token_excessive_input** | Info | per-request input > 30000 & per-request output < 1000 | Single API call token cost too high |

### View / 视图结构类

| Rule | Severity | Threshold | Logic |
|------|----------|-----------|-------|
| **view_mismatch** | Warn | — | Data integrity check: view.json `turns.len()` ≠ `jsonl_ids.len()` for this session. Each turn should correspond to exactly one API call; a mismatch signals a view construction bug in the proxy, not an agent problem. |

## API / 接口

### `POST /dashboard/api/sessions/{session_id}/diagnose`

Trigger diagnosis. Runs rules, writes `.diagnose.json`, returns `DiagnoseReport`.

### `GET /dashboard/api/sessions/{session_id}/diagnose`

Read existing `.diagnose.json`. Returns 404 if not yet diagnosed.

### `GET /dashboard/api/raw/{jsonl_stem}?ids=1,2,3`

Return matching raw JSONL lines for deep inspection in the UI.

## CLI

```bash
# Diagnose a single session
cargo run -- diagnose session_1779871837_1

# Output format
cargo run -- diagnose session_1779871837_1 --format json   # default
cargo run -- diagnose session_1779871837_1 --format terminal
```

Implementation: check `std::env::args().nth(1)` in `main.rs`. If it equals `"diagnose"`, run diagnosis and exit instead of starting the proxy server. (`args[0]` is the binary path; `args[1]` is the subcommand.)

## UI / 界面

### Dashboard — entry point on low-score items / 低分条目入口

Sessions with score < 0.6 (or ungraded) show diagnosis entry:

- **Not yet diagnosed:** `[Diagnose]` button → clicking triggers `POST /diagnose`
- **Diagnosed with issues:** `⚠ 3 issues` badge → clicking opens detail panel
- **Diagnosed clean:** `✓` badge

### Session Detail — diagnosis panel / 详情页诊断面板

Shown below the grade section:

```
┌─ Diagnosis ──────────────────────────────────────────┐
│ ⚠ 3 issues found (1 error, 2 warnings)               │
│                                                      │
│ 🔴 [Tool] tool_result_missing                        │
│    Turn 1, Step 2: tool_call 'read_file' has no      │
│    matching tool_result in subsequent turns.          │
│    Evidence: {"call_id":"call_abc123",...}            │
│    Location: jsonl_id=3, turn=1                       │
│                                                      │
│ 🟡 [Tool] tool_duplicate_3plus                       │
│    Tool 'search_code' called 4 times with identical   │
│    arguments.                                         │
│                                                      │
│ 🟡 [Prompt] prompt_bloat                             │
│    System prompt is 3847 chars (threshold: 3000).     │
│    Agent prompt design issue: overly long system      │
│    instructions waste context window.                 │
├──────────────────────────────────────────────────────┤
│ [View Raw JSONL ▸]  ← collapsible raw data panel     │
└──────────────────────────────────────────────────────┘
```

## Persistence / 持久化

```
logs/
  session_1779871837_1.view.json
  session_1779871837_1.grade.json
  session_1779871837_1.diagnose.json   ← NEW
```

`.diagnose.json` stores the full `DiagnoseReport`. The filename is the `session_id` itself, so no extra linking field is needed.

## Implementation Phases / 实现阶段

### Phase 1: Backend / 后端
1. Create `src/diagnose/` module (mod.rs + types.rs + rules.rs)
2. Implement `diagnose::run()` with all 10 rules
3. Implement `.diagnose.json` read/write
4. Register `POST` + `GET /dashboard/api/sessions/{id}/diagnose`
5. Register `GET /dashboard/api/raw/{jsonl_stem}`
6. Verify: `cargo build` + curl test

### Phase 2: CLI / 命令行
7. Add `diagnose` subcommand dispatch in `main.rs`
8. Support `--format terminal|json`
9. Verify: `cargo run -- diagnose <session_id>`

### Phase 3: UI / 界面
10. Dashboard: add `[Diagnose]` button / status badge on low-score (<0.6) items
11. Session Detail: add diagnosis panel with issue cards + raw JSONL expand
12. Verify: end-to-end browser flow

## Design Decisions / 设计决策

- **Pure rule engine, no LLM**: agent behavioral patterns are detectable via structural rules without AI judgment
- **Thresholds hardcoded**: prompt > 3000 chars, token input > 30000, token waste < 2%
- **All rules always on**: no toggle switches for initial version
- **CLI single-session only**: `--all` and `--filter` deferred to later iteration
- **Web ↔ CLI share the same module**: `diagnose::run()` is the single source of truth
