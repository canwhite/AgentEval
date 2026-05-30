# AgentEval Implementation / 系统实现文档

## Architecture Overview / 架构总览

```
                          ┌─────────────────────────────────┐
                          │         AgentEval Proxy          │
                          │  (axum HTTP server on :57633)     │
                          └──────────────┬──────────────────┘
                                         │
              ┌──────────────────────────┼──────────────────────────┐
              │                          │                          │
              ▼                          ▼                          ▼
     ┌──────────────┐          ┌──────────────┐          ┌──────────────┐
     │   Proxy       │          │   Dashboard  │          │   CLI         │
     │  (proxy.rs)   │          │  (web/mod.rs)│          │  (main.rs)   │
     │               │          │              │          │              │
     │ Transparent   │          │ Session list │          │ diagnose     │
     │ forward with  │          │ Detail view  │          │ subcommand   │
     │ recording     │          │ Grade/Diag   │          │              │
     └──────┬────────┘          └──────┬───────┘          └──────┬───────┘
            │                          │                          │
            │ TurnRecord               │ SessionView              │
            │ (unbounded channel)      │ GradeReport              │
            │                          │ DiagnoseReport           │
            ▼                          │                          │
     ┌──────────────┐                  │                          │
     │  Eval Engine │                  │                          │
     │ (eval/mod.rs)│                  │                          │
     │              │                  │                          │
     │ Build views  │                  │                          │
     │ Trigger      │                  │                          │
     │ grader       │                  │                          │
     └──────┬───────┘                  │                          │
            │                          │                          │
            │ SessionView              │                          │
            ▼                          ▼                          ▼
     ┌──────────────┐          ┌──────────────┐          ┌──────────────┐
     │    Grader    │          │   Diagnose   │          │    Format    │
     │ (grader/*.rs)│          │(diagnose/*.rs)│         │(format/*.rs) │
     │              │          │              │          │              │
     │ LLM judge +  │          │ 10 rule-     │          │ OpenAI msg   │
     │ rule metrics │          │ based checks │          │ parsing      │
     └──────────────┘          └──────────────┘          └──────────────┘
```

## Module Map / 模块关系

```
src/
├── main.rs          # Entry: server startup, CLI dispatch, route registration
├── config.rs        # Env-based config: upstream, port, log_dir, grader creds
├── proxy.rs         # Reverse proxy + traffic recorder → TurnRecord
├── web/
│   ├── mod.rs       # API handlers + session list logic
│   └── ui.html      # SPA dashboard (~680 lines vanilla JS, no framework)
├── eval/
│   ├── mod.rs       # SessionBuilder: TurnRecord → SessionView, session split
│   └── types.rs     # SessionView, Turn, Step, ToolResult, Usage
├── format/
│   ├── mod.rs       # Module root
│   └── openai.rs    # Parse OpenAI-format request/response → messages, steps, usage
├── grader/
│   ├── mod.rs       # run_pipeline: metrics → LLM judge → weighted grade
│   ├── types.rs     # GradeReport, DimensionScore, MetricsSnapshot, JudgeScores
│   ├── rules.rs     # extract_metrics, calc_tool_efficiency, calc_performance
│   ├── prompt.rs    # Build judge prompt from session summary
│   └── judge.rs     # Call LLM via reqwest, parse JSON response
└── diagnose/
    ├── mod.rs       # run(): view.json + .jsonl → DiagnoseReport → .diagnose.json
    ├── types.rs     # DiagnoseReport, DiagnoseIssue, IssueCategory, Severity
    └── rules.rs     # 10 diagnostic rules (4 tool, 2 prompt, 3 token, 1 view)
```

## Data Flow / 数据流

### Step 1: Recording (Proxy)

```
Client ──HTTP──▶ AgentEval Proxy ──forward──▶ Upstream API (DeepSeek/OpenAI)
                      │
                      │ Record: request_body, response_body, duration_ms
                      ▼
                 TurnRecord { id, request_body, response_body, duration_ms }
                      │
                      │ unbounded mpsc channel
                      ▼
                 eval::run() consumer
```

### Step 2: Session Construction (Eval)

```
TurnRecord stream
      │
      ▼
SessionBuilder::process()
  ├── parse_request_messages()  → prev/current diff → user_input, tool_results
  ├── parse_response_steps()    → Step::{Reasoning, Text, ToolCall}[]
  ├── parse_response_usage()    → Option<Usage>
  ├── cross-turn backfill       → pending_tool_calls matched with tool_results
  └── detect new session        → common_prefix_len <= 1 triggers seal
      │
      ▼
SessionView { session_id, model, turns[], jsonl_ids[] }
      │
      ├── write_view_json() → logs/{session_id}.view.json
      └── seal_and_grade_bg() → tokio::spawn grader::run_pipeline()
```

Session splitting: When `common_prefix_len(prev_messages, current_messages) <= 1`, the message history has been reset — a new session starts. Sessions are numbered `{jsonl_stem}_{counter}` where counter increments per split.

Timeout: After 120 seconds of inactivity, the current session is sealed.

### Step 3: Grading

```
GraderConfig { judge_api_base, judge_model, judge_api_key }
      │
      ▼
run_pipeline(view, config)
  ├── rules::extract_metrics(view) → MetricsSnapshot
  ├── prompt::format_session(view, metrics) → compact text
  ├── prompt::build_judge_prompt(text) → full judge prompt
  ├── judge::judge(config, prompt) → reqwest POST → JSON → JudgeScores
  │     │
  │     └── Fallback on error: rule-based estimation
  ├── rules::calc_tool_efficiency(metrics) → (score, reason)
  ├── rules::calc_performance(metrics) → (score, reason)
  └── weighted sum (weights: 0.35, 0.30, 0.20, 0.15) → GradeReport
      │
      ▼
write_grade_json() → logs/{session_id}.grade.json
```

### Step 4: Diagnosing

```
CLI: cargo run -- diagnose <session_id>
Web: POST /dashboard/api/sessions/{session_id}/diagnose
      │
      ▼
diagnose::run(session_id, log_dir)
  ├── read {session_id}.view.json → SessionView
  ├── split session_id on last '_' → jsonl_stem
  ├── read {jsonl_stem}.jsonl, filter by view.jsonl_ids
  └── rules::run_all(&view, &jsonl_entries)
        ├── tool_result_missing     (Error)
        ├── tool_result_error       (Error)
        ├── tool_duplicate_3plus    (Warn)
        ├── tool_result_empty       (Warn)
        ├── prompt_bloat            (Warn)
        ├── prompt_context_overflow (Error)
        ├── token_empty_response    (Error)
        ├── token_waste             (Warn)
        ├── token_excessive_input   (Info)
        └── view_mismatch           (Warn)
      │
      ▼
DiagnoseReport → write {session_id}.diagnose.json
```

## Key Types / 核心类型

### SessionView (eval/types.rs)

```rust
struct SessionView {
    session_id: String,       // "{jsonl_stem}_{counter}"
    model: String,            // extracted from request body
    upstream: String,         // always "" (set by proxy)
    jsonl_ids: Vec<u64>,      // references into the .jsonl file
    turns: Vec<Turn>,
}

struct Turn {
    turn_id: u64,             // 1-based, monotonically increasing
    user_input: Vec<String>,  // new user messages this turn
    tool_results: Vec<ToolResult>,  // tool outputs provided this turn
    steps: Vec<Step>,         // model output steps
    usage: Option<Usage>,     // token consumption
    duration_ms: u64,
}

enum Step {
    Reasoning { content: String },
    Text { content: String },
    ToolCall { call_id, name, arguments, result: Option<ToolResult> },
}
```

### GradeReport (grader/types.rs)

```rust
struct GradeReport {
    session_id: String,
    model: String,
    jsonl_ids: Vec<u64>,
    turn_count: usize,
    dimensions: Vec<DimensionScore>,  // 4 dimensions
    overall: f64,                     // weighted sum, [0.0, 1.0]
}

struct DimensionScore {
    metric: String,     // task_completion | tool_efficiency | response_quality | performance
    score: f64,
    source: String,     // "llm" | "rule"
    reason: String,
    details: Value,
    weight: f64,        // 0.35 | 0.30 | 0.20 | 0.15
}
```

### DiagnoseReport (diagnose/types.rs)

```rust
struct DiagnoseReport {
    session_id: String,
    diagnosed_at: String,         // ISO 8601
    summary: DiagnoseSummary,     // { total_issues, errors, warnings, infos }
    issues: Vec<DiagnoseIssue>,
}

struct DiagnoseIssue {
    category: IssueCategory,      // Tool | Prompt | Token | View
    severity: Severity,           // Error | Warn | Info
    title: String,                // rule name
    detail: String,               // human-readable explanation
    location: IssueLocation,      // { jsonl_id?, turn_id?, step_index? }
    evidence: String,             // truncated raw data excerpt
}
```

## API Endpoints / 接口设计

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/dashboard/` | Serve SPA (static HTML) |
| `GET` | `/dashboard/api/sessions` | List all sessions with grade + diagnose summaries |
| `GET` | `/dashboard/api/sessions/{id}` | Get session detail (view + grade) |
| `POST` | `/dashboard/api/sessions/{id}/grade` | Trigger grading, return report |
| `POST` | `/dashboard/api/sessions/{id}/diagnose` | Trigger diagnosis, return report |
| `GET` | `/dashboard/api/sessions/{id}/diagnose` | Read existing diagnosis (404 if none) |
| `GET` | `/dashboard/api/raw/{jsonl_stem}?ids=1,2,3` | Fetch raw JSONL entries for inspection |
| `*` | `/*` | Proxy all other requests to upstream |

## Grading Dimensions / 评分维度

| Dimension | Weight | Source | What it measures |
|-----------|--------|--------|-----------------|
| `task_completion` | 0.35 | LLM judge | Did the agent complete the user's task? |
| `tool_efficiency` | 0.30 | Rule-based | Tool errors, duplicates, call patterns |
| `response_quality` | 0.20 | LLM judge | Is the response helpful, accurate, well-structured? |
| `performance` | 0.15 | Rule-based | Token consumption, latency, turn count |

LLM judge degradation: If the judge API call fails, rule-based fallback estimates are used:
- Task completion: 0.7 (has text + no errors), 0.3 (has errors), 0.4 (ambiguous)
- Response quality: 0.6 (reply > 100 chars), 0.3 (short/no reply)

### Tool Efficiency Rules (grader/rules.rs)

| Signal | Penalty |
|--------|---------|
| `tool_error_count > 0` | -0.2 per error (floor 0.3) |
| `duplicate_tool_calls > 0` | -0.05 per duplicate (floor 0.5) |
| No tool calls at all | +0.1 bonus (simple tasks are efficient) |
| Baseline | 0.85 |

### Performance Rules (grader/rules.rs)

| Signal | Penalty |
|--------|---------|
| `total_input_tokens > 20_000` | -0.2 |
| `total_input_tokens > 50_000` | -0.4 |
| `avg_duration > 10_000ms` | -0.15 |
| `avg_duration > 20_000ms` | -0.3 |
| `turn_count > 10` | -0.1 |
| `turn_count > 20` | -0.25 |
| Baseline | 1.0 |

## Diagnostic Rules Reference / 诊断规则参考

### Tool Rules (data source: SessionView)

| Rule | Severity | Threshold | What it detects |
|------|----------|-----------|-----------------|
| `tool_result_missing` | Error | — | Tool call result never arrived (broken tool chain) |
| `tool_result_error` | Error | — | Tool execution returned error |
| `tool_duplicate_3plus` | Warn | >= 3 calls | Same tool+args called repeatedly (retry loop) |
| `tool_result_empty` | Warn | — | Tool ran but produced empty output |

### Prompt Rules (data source: JSONL request bodies)

| Rule | Severity | Threshold | What it detects |
|------|----------|-----------|-----------------|
| `prompt_bloat` | Warn | > 3000 chars | System prompt too long, wastes context |
| `prompt_context_overflow` | Error | — | Orphan tool_call_id in message history (truncation) |

### Token Rules

| Rule | Severity | Data Source | Threshold |
|------|----------|-------------|-----------|
| `token_empty_response` | Error | JSONL response_body | No content AND no tool calls |
| `token_waste` | Warn | SessionView Turn.usage | output/input < 2% AND input > 5000 |
| `token_excessive_input` | Info | SessionView Turn.usage | input > 30000 AND output < 1000 |

### View Rule

| Rule | Severity | Threshold | What it detects |
|------|----------|-----------|-----------------|
| `view_mismatch` | Warn | turns.len() != jsonl_ids.len() | Data integrity: view construction bug |

## File Persistence / 文件持久化

```
{AGENTEVAL_LOG_DIR}/   (default: ~/.agenteval/logs/)
├── {stem}.jsonl                       # Raw recorded traffic (append-only)
├── {stem}_{N}.view.json               # Structured session view
├── {stem}_{N}.grade.json              # Grading report
└── {stem}_{N}.diagnose.json           # Diagnosis report
```

All output files use `serde_json::to_string_pretty` for readability. File writes are best-effort (errors logged, not propagated).

## Config / 配置

All configuration via environment variables (`.env` supported via dotenvy):

| Variable | Default | Purpose |
|----------|---------|---------|
| `AGENTEVAL_UPSTREAM` | `https://api.deepseek.com` | Upstream API base URL |
| `AGENTEVAL_PORT` | `57633` | Proxy listen port |
| `AGENTEVAL_LOG_DIR` | `~/.agenteval/logs` | Log/output directory |
| `AGENTEVAL_UI_ENABLED` | `true` | Enable dashboard routes |
| `AGENTEVAL_VERBOSE` | `false` | Verbose logging |
| `AGENTEVAL_JUDGE_API_BASE` | same as UPSTREAM | Judge LLM API base |
| `AGENTEVAL_JUDGE_MODEL` | `MiniMax-M2.5` | Judge model name |
| `AGENTEVAL_JUDGE_API_KEY` | (empty) | Judge API key |

## Design Decisions / 设计决策

1. **No framework dependencies**: Dashboard is vanilla JS (no React/Vue). Server is plain axum. Zero build step for UI changes.

2. **Session split by message history**: `common_prefix_len <= 1` detects when the upstream conversation resets. No explicit session management protocol needed — works with any client.

3. **Unbounded channel for recording**: `mpsc::unbounded_channel` avoids backpressure on the proxy path. Eval processing happens asynchronously.

4. **Rule engine before LLM**: Diagnose is a pure rule engine (no LLM). Grader uses LLM for quality assessment but rules for efficiency and performance. This keeps costs predictable.

5. **Streaming support**: Both non-streaming (JSON) and streaming (SSE) responses are parsed correctly. The format module handles the distinction transparently.

6. **Backfill pattern**: Tool call results arrive in subsequent requests. The eval engine registers pending calls and backfills `Step::ToolCall.result` when matching tool messages arrive. This gives a complete picture per-turn.

7. **SPA with pushState**: Dashboard uses `window.history.pushState` + `popstate` for client-side routing. No server-side session state needed.

8. **Graceful degradation**: If the judge LLM fails, grading falls back to rule-based estimates. If diagnose.json doesn't exist yet, the UI shows a "Run Diagnosis" button.

## Known Limitations / 已知限制

| Limitation | Detail |
|-----------|--------|
| `is_error` not populated | `ToolResult.is_error` is always `false` in the standard eval pipeline. The `tool_result_error` rule is structurally correct but will not fire via the current data path. Upstream agents would need to signal errors explicitly. |
| SSE detection heuristic | Uses `s.contains("data: ")` to distinguish SSE from JSON. Could theoretically false-match if JSON content contains the literal string "data: ". |
| JSONL memory load | Entire JSONL file is read into memory for relevant line extraction. Acceptable for typical session sizes; would need streaming parser for very large files. |
| No multi-session CLI | `cargo run -- diagnose` operates on one session at a time. Batch diagnosis (`--all`) deferred. |
| Single grader model | All dimensions use one judge model. Dimension-specific models (e.g., cheaper model for response_quality) not supported yet. |
