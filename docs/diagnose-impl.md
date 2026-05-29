# Diagnose Module Implementation / 诊断模块实现记录

## Overview / 概述

实现了完整的 Session 嗅探诊断功能：后端规则引擎 + CLI 入口 + Web API + Dashboard UI。

以单个 view.json 为主要分析对象，穿透原始 JSONL 获取补充上下文，通过 10 条纯规则检测 Agent 的行为问题（工具使用、Prompt 设计、Token 消耗等）。

## Files Created / 新增文件

```
src/diagnose/
  mod.rs       # run() 主入口 + 文件读写 + read_raw_jsonl()
  types.rs     # DiagnoseReport, DiagnoseIssue, IssueCategory, Severity
  rules.rs     # 10 条诊断规则 + JsonlEntry 上下文类型
```

## Files Modified / 修改文件

| File | Change |
|------|--------|
| `Cargo.toml` | Added `chrono = "0.4"` dependency |
| `src/main.rs` | Registered `mod diagnose`; added CLI `diagnose` subcommand dispatch; registered 2 new API routes |
| `src/web/mod.rs` | Added `diagnose_summary` field to `SessionSummary`; added `read_diagnose_summary()` helper; added 3 handlers: `diagnose_session`, `get_diagnose`, `get_raw_jsonl` |
| `src/web/ui.html` | Dashboard diagnose badge/button; detail view diagnosis panel; Re-diagnose; CSS for diagnose components |

## Module: `src/diagnose/types.rs`

```rust
struct DiagnoseReport {
    session_id: String,
    diagnosed_at: String,       // ISO 8601
    summary: DiagnoseSummary,
    issues: Vec<DiagnoseIssue>,
}

struct DiagnoseSummary {
    total_issues: usize,
    errors: usize,    // Severity::Error count
    warnings: usize,  // Severity::Warn count
    infos: usize,     // Severity::Info count
}

struct DiagnoseIssue {
    category: IssueCategory,    // Tool | Prompt | Token | View
    severity: Severity,         // Error | Warn | Info
    title: String,              // rule name, e.g. "tool_result_missing"
    detail: String,             // human-readable description
    location: IssueLocation,    // jsonl_id, turn_id, step_index
    evidence: String,           // relevant raw data excerpt (truncated)
}

enum IssueCategory { Tool, Prompt, Token, View }
enum Severity { Error, Warn, Info }
struct IssueLocation { jsonl_id: Option<u64>, turn_id: Option<u64>, step_index: Option<usize> }
```

## Module: `src/diagnose/rules.rs`

### Rule entry point

`run_all(view: &SessionView, jsonl_entries: &[JsonlEntry]) -> Vec<DiagnoseIssue>`

Orchestrates all 10 rules. Tool rules operate on view.json turns/steps; Prompt and Token rules access raw JSONL request/response bodies; View rule checks data integrity.

### Tool rules (4)

| Rule | Severity | Data Source | Detection |
|------|----------|-------------|-----------|
| `tool_result_missing` | Error | view turns/steps | `Step::ToolCall.result` is `None` (not backfilled cross-turn) |
| `tool_result_error` | Error | view turns | `ToolResult.is_error == true` (inline + backfilled) |
| `tool_duplicate_3plus` | Warn | view turns/steps | Same `name` + structurally-equal `arguments` counted; flag if ≥ 3 occurrences |
| `tool_result_empty` | Warn | view turns | `ToolResult.content.trim().is_empty()` (inline + backfilled) |

`tool_duplicate_3plus` uses `normalize_args()` for canonical JSON comparison — object keys are sorted before stringification to handle equivalent-but-differently-ordered arguments.

### Prompt rules (2)

| Rule | Severity | Data Source | Detection |
|------|----------|-------------|-----------|
| `prompt_bloat` | Warn | JSONL `request_body.messages[].role="system"` | System prompt `content` > 3000 chars |
| `prompt_context_overflow` | Error | JSONL `request_body.messages[]` | Tool message's `tool_call_id` has no matching `tool_calls[].id` in any assistant message within the same request — indicates the assistant message was truncated from history |

`prompt_bloat` only checks the **first** JSONL entry for the session (system prompt is invariant).

### Token rules (3)

| Rule | Severity | Data Source | Detection |
|------|----------|-------------|-----------|
| `token_empty_response` | Error | JSONL `response_body` | Handles both SSE (string) and JSON (object) response bodies. Checks for empty content AND no tool calls. |
| `token_waste` | Warn | view `Turn.usage` | `output / input < 2%` AND `input > 5000` tokens |
| `token_excessive_input` | Info | view `Turn.usage` | `input > 30000` AND `output < 1000` tokens |

`token_empty_response` has a dedicated `sse_has_content()` parser that walks SSE `data:` lines checking delta `content` and `tool_calls`.

### View rule (1)

| Rule | Severity | Data Source | Detection |
|------|----------|-------------|-----------|
| `view_mismatch` | Warn | view.json | `turns.len() != jsonl_ids.len()` — data integrity check, not an agent problem |

## Module: `src/diagnose/mod.rs`

### `run(session_id, log_dir) -> Result<DiagnoseReport, String>`

1. Read `{log_dir}/{session_id}.view.json` → deserialize `SessionView`
2. Derive `jsonl_stem` from `session_id` by splitting on the **last** `_`
3. Read `{log_dir}/{jsonl_stem}.jsonl`, filter lines by `view.jsonl_ids`
4. Call `rules::run_all(&view, &jsonl_entries)`
5. Build `DiagnoseReport` with ISO timestamp from `chrono::Utc`
6. Write `{log_dir}/{session_id}.diagnose.json`
7. Return report

### `read_existing(session_id, log_dir) -> Result<DiagnoseReport, String>`

Reads an existing `.diagnose.json`. Returns error string if not found or unparseable.

### `read_raw_jsonl(jsonl_stem, ids, log_dir) -> Result<Vec<Value>, String>`

Reads full JSONL entries by IDs. Used by the raw JSONL API endpoint for UI deep inspection.

## API Endpoints / 接口

### `POST /dashboard/api/sessions/{session_id}/diagnose`

Triggers diagnosis synchronously. Runs rules, writes `.diagnose.json`, returns `DiagnoseReport`.

Handler: `web::diagnose_session()`

### `GET /dashboard/api/sessions/{session_id}/diagnose`

Reads existing `.diagnose.json`. Returns 404 if not yet diagnosed.

Handler: `web::get_diagnose()`

### `GET /dashboard/api/raw/{jsonl_stem}?ids=1,2,3`

Returns matching raw JSONL lines as `{ "entries": [...] }`.

Handler: `web::get_raw_jsonl()`

### Session list enhancement

`GET /dashboard/api/sessions` now includes `diagnose_summary` field in each `SessionSummary` when a `.diagnose.json` exists:

```json
{
  "session_id": "session_1779871837_1",
  "overall": 0.9,
  "graded": true,
  "diagnose_summary": {
    "total_issues": 2,
    "errors": 0,
    "warnings": 2,
    "infos": 0
  }
}
```

This enables the dashboard to show diagnose status immediately on page load without extra API calls.

## CLI / 命令行

```bash
# Diagnose a single session (JSON output)
cargo run -- diagnose session_1779871837_1

# Terminal-formatted output with icons
cargo run -- diagnose session_1779871837_1 --format terminal
```

Implementation in `main.rs`: checks `std::env::args()` before starting the proxy server. If `args[1] == "diagnose"`, dispatches to `run_diagnose_cli()` and exits. Log directory comes from `AGENTEVAL_LOG_DIR` env var (default: `./logs`).

## UI / 界面

### Dashboard — diagnose badge

- Sessions with score < 0.6 (or ungraded) show a diagnose column
- **Not yet diagnosed**: `[Diagnose]` button → clicking POSTs to diagnose endpoint, then updates badge inline
- **Diagnosed with issues**: `⚠ N issues` badge (clickable, navigates to detail view)
- **Diagnosed clean**: `✓ clean` badge
- State persists across page refreshes via `diagnose_summary` in the session list API

### Session Detail — diagnosis panel

Below the grade section:
- If diagnosed: full issue card list with category badges, severity colors, detail text, location, and collapsible evidence
- If not diagnosed: `[🔍 Run Diagnosis]` button → runs and renders inline
- Header row: `🔍 Diagnosis` title + `[🔄 Re-diagnose]` button (top-right, no scrolling needed)
- Re-diagnose replaces panel content inline with fresh results

### CSS classes added

`.diag-badge`, `.diag-btn`, `.diag-issues`, `.diag-clean`, `.diag-pending`, `.diag-panel`, `.diag-issue-card`, `.sev-error`, `.sev-warn`, `.sev-info`, `.diag-issue-title`, `.diag-issue-cat`, `.cat-tool`, `.cat-prompt`, `.cat-token`, `.cat-view`, `.diag-issue-detail`, `.diag-issue-loc`, `.diag-issue-evidence`

### JavaScript functions added

| Function | Purpose |
|----------|---------|
| `diagCell(s)` | Returns dashboard cell HTML for diagnose status |
| `triggerDiagnose(sid, el)` | POSTs diagnose from dashboard, updates cache, re-renders |
| `loadDiagnose(sid)` | GETs existing diagnose report (returns null if 404) |
| `renderDiagnosePanel(report, sid)` | Renders full diagnose panel HTML with issue cards |
| `triggerDiagnoseDetail(sid)` | Runs diagnose from detail view, renders panel inline |
| `triggerReDiagnose(sid)` | Re-runs diagnose, replaces panel content inline |

### Client-side cache (`window._diagCache`)

Maps `session_id` → `DiagnoseSummary | null`. Populated by `triggerDiagnose` and `triggerReDiagnose`. `diagCell()` checks `s.diagnose_summary` (server-provided) first, falls back to `window._diagCache`.

## Data Flow / 数据流

```
                    ┌──────────────────────┐
   Dashboard        │  diagCell(s)         │
   page load        │  reads:              │
                    │  s.diagnose_summary  │ ← list_sessions checks .diagnose.json
                    │  || _diagCache[sid]  │ ← in-memory cache
                    └──────────────────────┘
                              │
                 [Diagnose] button clicked
                              │
                              ▼
                    POST /api/sessions/{id}/diagnose
                              │
                    ┌─────────┴─────────┐
                    │  diagnose::run()  │
                    │  read .view.json  │
                    │  read .jsonl      │
                    │  run 10 rules     │
                    │  write .diagnose  │
                    └─────────┬─────────┘
                              │
                    ┌─────────┴──────────┐
                    ▼                    ▼
              Dashboard            Detail View
              badge updates        panel renders
              (_diagCache)         (renderDiagnosePanel)
```

## Persistence / 持久化

```
logs/
  session_1779871837_1.view.json
  session_1779871837_1.grade.json
  session_1779871837_1.diagnose.json   ← NEW
```

`.diagnose.json` stores the full `DiagnoseReport`. Filename is the `session_id` itself.

## Bug Found During Review / 审查中发现的 Bug

**Cache shape inconsistency**: `triggerDiagnose()` stored the full `DiagnoseReport` object in `window._diagCache`, but `diagCell()` expected a flat summary with `.total_issues` at the top level. After clicking `[Diagnose]` on the dashboard, the badge incorrectly showed "✓ clean" because `report.total_issues` is `undefined` (it's nested at `report.summary.total_issues`).

**Fix**: Changed `triggerDiagnose()` to store `report.summary` instead of `report`. Also normalized the error fallback to use the same flat shape. All four cache-write sites now consistently store `{ total_issues, errors, warnings, infos }`.
