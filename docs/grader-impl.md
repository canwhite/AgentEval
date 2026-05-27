# AgentEval Grader 实现

> 关键代码源自 `src/grader/` 和 `src/eval/mod.rs`，与 `docs/grader-design.md` 方案对应。

---

## 一、Session 边界检测 (`src/eval/mod.rs`)

### 核心算法：消息数组 diff

```rust
fn common_prefix_len(prev: &[Value], current: &[Value]) -> usize {
    prev.iter()
        .zip(current.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

fn is_new_session(prev: &[Value], current: &[Value]) -> bool {
    common_prefix_len(prev, current) <= 1
}
```

### 主循环：select! 同时监听新请求和 2 分钟超时

```rust
pub async fn run(
    mut rx: UnboundedReceiver<TurnRecord>,
    log_dir: String,
    jsonl_stem: String,
    grader_config: GraderConfig,
) {
    let mut builder: Option<SessionBuilder> = None;
    let mut session_counter: u32 = 0;

    loop {
        let record = tokio::select! {
            recv = rx.recv() => recv,
            _ = tokio::time::sleep(Duration::from_secs(120)) => {
                if let Some(ref b) = builder {
                    session_counter += 1;
                    seal_and_grade_bg(b, &log_dir, &jsonl_stem, session_counter, &grader_config);
                    builder = None;
                }
                continue;
            }
        };

        let record = match record {
            Some(r) => r,
            None => break,
        };

        if record.request_body.is_null() {
            continue;
        }

        // 检测 message 回退 → 新 session
        if let Some(ref b) = builder {
            let current_msgs = format::openai::parse_request_messages(&record.request_body);
            if is_new_session(&b.prev_messages, &current_msgs) {
                eprintln!("[eval] id={:04} -> NEW SESSION detected, sealing + bg grade", record.id);
                session_counter += 1;
                seal_and_grade_bg(b, &log_dir, &jsonl_stem, session_counter, &grader_config);
                builder = None;
            }
        }

        if builder.is_none() {
            let model = extract_model(&record.request_body);
            let session_id = format!("{}_{}", jsonl_stem, session_counter + 1);
            builder = Some(SessionBuilder::new(session_id, model));
        }

        let b = builder.as_mut().unwrap();
        b.process(&record.request_body, &record.response_body, record.duration_ms, record.id);
        write_view_json(b, &log_dir);
    }

    // channel 关闭 → 最后一个 session 同步评分
    if let Some(ref b) = builder {
        if !b.turns.is_empty() {
            session_counter += 1;
            let final_id = format!("{}_{}", jsonl_stem, session_counter);
            let view = build_with_id(b, &final_id);
            write_view_final(&view, &log_dir);
            let report = grader::run_pipeline(&view, &grader_config).await;
            write_grade_json(&report, &log_dir);
        }
    }
}
```

### 后台评分：view 立即落盘，grade 异步 spawn

```rust
fn seal_and_grade_bg(
    builder: &SessionBuilder,
    log_dir: &str,
    jsonl_stem: &str,
    counter: u32,
    grader_config: &GraderConfig,
) {
    let final_id = format!("{}_{}", jsonl_stem, counter);
    let view = build_with_id(builder, &final_id);
    write_view_final(&view, log_dir);

    eprintln!("[eval] session {} sealed ({} turns), grading in background...",
        final_id, view.turns.len());

    let grader_config = grader_config.clone();
    let log_dir = log_dir.to_string();
    tokio::spawn(async move {
        let report = grader::run_pipeline(&view, &grader_config).await;
        write_grade_json(&report, &log_dir);
        eprintln!("[eval] grade written to {}/{}.grade.json (overall: {})",
            log_dir, final_id, report.overall);
    });
}
```

---

## 二、流水线编排 (`src/grader/mod.rs`)

```rust
pub async fn run_pipeline(view: &SessionView, config: &GraderConfig) -> GradeReport {
    let metrics = rules::extract_metrics(view);

    // Step 2: LLM 评审，失败则降级为规则推算
    let (tc_score, tc_reason, rq_score, rq_reason) = match judge_llm(view, &metrics, config).await {
        Ok(scores) => (
            scores.task_completion.score.clamp(0.0, 1.0),
            scores.task_completion.reason,
            scores.response_quality.score.clamp(0.0, 1.0),
            scores.response_quality.reason,
        ),
        Err(e) => {
            eprintln!("[grader] LLM judge failed: {}, falling back to rule-based", e);
            let tc = if metrics.has_final_text && !metrics.has_tool_error {
                (0.7, "有文本回复且无错误（降级估算）".to_string())
            } else if metrics.has_tool_error {
                (0.3, "存在工具错误（降级估算）".to_string())
            } else {
                (0.4, "无明显终态（降级估算）".to_string())
            };
            let rq = if metrics.final_text_len > 100 {
                (0.6, "回复篇幅尚可（降级估算）".to_string())
            } else {
                (0.3, "回复较短或无回复（降级估算）".to_string())
            };
            (tc.0, tc.1, rq.0, rq.1)
        }
    };

    // Step 3: 规则维度 + 加权汇总
    let (tool_score, tool_reason) = rules::calc_tool_efficiency(&metrics);
    let (perf_score, perf_reason) = rules::calc_performance(&metrics);

    let dimensions = vec![
        DimensionScore {
            metric: "task_completion".into(), score: tc_score,
            source: "llm".into(), reason: tc_reason, details: json!({}), weight: 0.35,
        },
        DimensionScore {
            metric: "tool_efficiency".into(), score: tool_score,
            source: "rule".into(), reason: tool_reason,
            details: json!({"total_calls": metrics.total_tool_calls,
                             "error_count": metrics.tool_error_count,
                             "duplicate_count": metrics.duplicate_tool_calls}),
            weight: 0.30,
        },
        DimensionScore {
            metric: "response_quality".into(), score: rq_score,
            source: "llm".into(), reason: rq_reason, details: json!({}), weight: 0.20,
        },
        DimensionScore {
            metric: "performance".into(), score: perf_score,
            source: "rule".into(), reason: perf_reason,
            details: json!({"total_tokens_in": metrics.total_input_tokens,
                             "total_tokens_out": metrics.total_output_tokens,
                             "avg_duration_ms": metrics.avg_duration_ms,
                             "turn_count": metrics.turn_count}),
            weight: 0.15,
        },
    ];

    let overall: f64 = dimensions.iter().map(|d| d.score * d.weight).sum();

    GradeReport {
        session_id: view.session_id.clone(),
        model: view.model.clone(),
        jsonl_ids: view.jsonl_ids.clone(),
        turn_count: view.turns.len(),
        dimensions,
        overall: (overall * 100.0).round() / 100.0,
    }
}
```

---

## 三、Step 1: 规则统计 (`src/grader/rules.rs`)

### extract_metrics

```rust
pub fn extract_metrics(view: &SessionView) -> MetricsSnapshot {
    let mut total_calls = 0usize;
    let mut error_count = 0usize;
    let mut duplicate_count = 0usize;
    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut total_duration: u64 = 0;
    let mut has_tool_error = false;
    let mut final_text_len: usize = 0;
    let mut has_final_text = false;
    let mut call_signatures: Vec<String> = Vec::new();

    for turn in &view.turns {
        total_duration += turn.duration_ms;
        if let Some(ref usage) = turn.usage {
            total_input += usage.input_tokens;
            total_output += usage.output_tokens;
        }
        for step in &turn.steps {
            match step {
                Step::Reasoning { .. } => {}
                Step::Text { content } => {
                    if turn.turn_id == view.turns.len() as u64 {
                        final_text_len += content.len();
                        has_final_text = true;
                    }
                }
                Step::ToolCall { name, arguments, result, .. } => {
                    total_calls += 1;
                    let sig = format!("{}:{}", name, arguments);
                    if call_signatures.contains(&sig) {
                        duplicate_count += 1;
                    } else {
                        call_signatures.push(sig);
                    }
                    if let Some(r) = result {
                        if r.is_error || r.content.starts_with("Error:") {
                            error_count += 1;
                            has_tool_error = true;
                        }
                    }
                }
            }
        }
    }

    MetricsSnapshot {
        total_tool_calls: total_calls,
        tool_success_count: total_calls.saturating_sub(error_count),
        tool_error_count: error_count,
        duplicate_tool_calls: duplicate_count,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        avg_duration_ms: if view.turns.len() > 0 { total_duration / view.turns.len() as u64 } else { 0 },
        turn_count: view.turns.len(),
        final_text_len,
        has_final_text,
        has_tool_error,
    }
}
```

### calc_tool_efficiency

```rust
pub fn calc_tool_efficiency(m: &MetricsSnapshot) -> (f64, String) {
    if m.total_tool_calls == 0 {
        return (1.0, "无工具调用".to_string());
    }
    let success_rate = m.tool_success_count as f64 / m.total_tool_calls as f64;
    let dup_penalty = (m.duplicate_tool_calls as f64 * 0.1).min(0.5);
    let score = (success_rate - dup_penalty).max(0.0).min(1.0);
    // ...
    (score, reason)
}
```

### calc_performance

```rust
pub fn calc_performance(m: &MetricsSnapshot) -> (f64, String) {
    // token 比值：output/input 在 0.05~0.3 最佳
    let token_ratio = if m.total_input_tokens > 0 {
        m.total_output_tokens as f64 / m.total_input_tokens as f64
    } else { 0.0 };
    let token_score = if token_ratio < 0.01 { 0.3 }
        else if token_ratio > 0.5 { 0.5 }
        else { 1.0 - ((token_ratio - 0.15).abs() * 2.0).min(0.7) };

    // 延迟：<10s 满分，10~30s 八折，>30s 五折
    let latency_score = if m.avg_duration_ms < 10_000 { 1.0 }
        else if m.avg_duration_ms < 30_000 { 0.8 }
        else { 0.5 };

    // turn 数：3~15 最佳
    let turn_score = match m.turn_count {
        0..=2 => 0.7, 3..=15 => 1.0, _ => 0.6,
    };

    let score = (token_score * 0.3 + latency_score * 0.4 + turn_score * 0.3).min(1.0);
    (score, reason)
}
```

---

## 四、Step 2: LLM 评审

### 会话摘要 (`src/grader/prompt.rs`)

```rust
pub fn format_session(view: &SessionView, metrics: &MetricsSnapshot) -> String {
    let mut buf = String::new();

    buf.push_str("## 会话概览\n");
    buf.push_str(&format!("- 模型: {}\n", view.model));
    buf.push_str(&format!("- Turn 数: {}\n", metrics.turn_count));
    buf.push_str(&format!("- Token: {} in / {} out\n", metrics.total_input_tokens, metrics.total_output_tokens));
    buf.push_str(&format!("- 工具调用: {} 次\n", metrics.total_tool_calls));
    buf.push_str(&format!("- 平均耗时: {}ms\n\n", metrics.avg_duration_ms));

    buf.push_str("## 对话记录\n");
    for turn in &view.turns {
        for u in &turn.user_input {
            buf.push_str(&format!("用户: \"{}\"\n", truncate(u, 500)));
        }
        for step in &turn.steps {
            match step {
                Step::Reasoning { content } =>
                    buf.push_str(&format!("  [思考] {}\n", truncate(content, 300))),
                Step::ToolCall { name, arguments, result, .. } => {
                    buf.push_str(&format!("  [工具调用] {}({})\n", name, truncate(&arguments.to_string(), 200)));
                    if let Some(r) = result {
                        let label = if r.is_error { "错误" } else { "结果" };
                        buf.push_str(&format!("  [工具{}] {}\n", label, truncate(&r.content, 500)));
                    }
                }
                Step::Text { content } =>
                    buf.push_str(&format!("  [回复] {}\n", truncate(content, 1000))),
            }
        }
        if let Some(ref usage) = turn.usage {
            buf.push_str(&format!("  ({} tokens in, {} tokens out, {}ms)\n",
                usage.input_tokens, usage.output_tokens, turn.duration_ms));
        }
        buf.push('\n');
    }
    buf
}
```

评测 prompt 见 `build_judge_prompt()`，要求 LLM 从 task_completion 和 response_quality 两个维度打分，输出严格 JSON。

### 调用评测 LLM (`src/grader/judge.rs`)

独立 `reqwest::Client`，`no_proxy()` 避免套娃：

```rust
pub async fn judge(config: &GraderConfig, prompt: &str) -> Result<JudgeScores, String> {
    let client = Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| format!("judge client build error: {}", e))?;

    let resp = client
        .post(&format!("{}/v1/chat/completions", config.judge_api_base))
        .header("Authorization", format!("Bearer {}", config.judge_api_key))
        .json(&serde_json::json!({
            "model": config.judge_model,
            "messages": [{ "role": "user", "content": prompt }],
            "temperature": 0.1,
            "max_tokens": 300
        }))
        .send()
        .await
        .map_err(|e| format!("judge request error: {}", e))?;

    let content = raw
        .get("choices").and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| format!("unexpected judge response structure"))?;

    let json_str = extract_json_block(content);
    serde_json::from_str::<JudgeScores>(&json_str)
}
```

### JSON 容错解析

处理 LLM 可能返回的四种格式：裸 JSON、```json 代码块、无标记代码块、混杂文本。

```rust
fn extract_json_block(text: &str) -> String {
    let text = text.trim();
    if text.starts_with('{') { return text.to_string(); }
    if let Some(start) = text.find("```json") { /* 提取 ```json ... ``` */ }
    if let Some(start) = text.find("```") { /* 提取 ``` ... ``` */ }
    if let (Some(s), Some(e)) = (text.find('{'), text.rfind('}')) { return text[s..=e].to_string(); }
    text.to_string()
}
```

---

## 五、数据结构 (`src/grader/types.rs`)

```rust
pub struct GradeReport {
    pub session_id: String,
    pub model: String,
    pub jsonl_ids: Vec<u64>,
    pub turn_count: usize,
    pub dimensions: Vec<DimensionScore>,
    pub overall: f64,
}

pub struct DimensionScore {
    pub metric: String,     // "task_completion" | "tool_efficiency" | ...
    pub score: f64,
    pub source: String,     // "rule" | "llm"
    pub reason: String,
    pub details: Value,
    pub weight: f64,
}

pub struct MetricsSnapshot {
    pub total_tool_calls: usize,
    pub tool_success_count: usize,
    pub tool_error_count: usize,
    pub duplicate_tool_calls: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub avg_duration_ms: u64,
    pub turn_count: usize,
    pub final_text_len: usize,
    pub has_final_text: bool,
    pub has_tool_error: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JudgeScores {
    pub task_completion: JudgeScoreItem,
    pub response_quality: JudgeScoreItem,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JudgeScoreItem {
    pub score: f64,
    pub reason: String,
}
```

---

## 六、配置 (`src/config.rs`)

```rust
#[derive(Clone)]
pub struct GraderConfig {
    pub judge_api_base: String,
    pub judge_model: String,
    pub judge_api_key: String,
}

impl GraderConfig {
    pub fn load(upstream: &str) -> Self {
        dotenvy::dotenv().ok();
        let judge_api_base = env::var("AGENTEVAL_JUDGE_API_BASE")
            .unwrap_or_else(|_| upstream.to_string())
            .trim_end_matches('/')
            .to_string();
        let judge_model = env::var("AGENTEVAL_JUDGE_MODEL")
            .unwrap_or_else(|_| "MiniMax-M2.5".to_string());
        let judge_api_key = env::var("AGENTEVAL_JUDGE_API_KEY").unwrap_or_default();
        Self { judge_api_base, judge_model, judge_api_key }
    }
}
```

---

## 七、输出示例

`logs/session_1768000000_1.grade.json`：

```json
{
  "session_id": "session_1768000000_1",
  "model": "MiniMax-M2.5",
  "jsonl_ids": [2, 3, 4],
  "turn_count": 3,
  "dimensions": [
    {
      "metric": "task_completion",
      "score": 0.85, "source": "llm",
      "reason": "agent 成功读取了 README 并向用户展示了内容，任务完成",
      "details": {}, "weight": 0.35
    },
    {
      "metric": "tool_efficiency",
      "score": 0.90, "source": "rule",
      "reason": "2 次调用全部成功，无重复",
      "details": { "total_calls": 2, "error_count": 0, "duplicate_count": 0 },
      "weight": 0.30
    },
    {
      "metric": "response_quality",
      "score": 0.70, "source": "llm",
      "reason": "回复基本准确但较多冗余内容",
      "details": {}, "weight": 0.20
    },
    {
      "metric": "performance",
      "score": 0.88, "source": "rule",
      "reason": "15200 tokens in / 1200 tokens out, avg 1800ms, 3 turns",
      "details": { "total_tokens_in": 15200, "total_tokens_out": 1200, "avg_duration_ms": 1800, "turn_count": 3 },
      "weight": 0.15
    }
  ],
  "overall": 0.84
}
```
