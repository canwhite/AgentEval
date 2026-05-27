# AgentEval Grader 方案

## 背景

eval 模块已完成：原始 API 流量 → SessionView（结构化 turn 序列，tool call 已配对）。
问题：
1. session 边界靠代理重启划分，多次对话混成一个 session
2. 无评分能力

## 目标

1. **自动切分 session**：代理进程内检测对话边界，新对话触发旧 session 封口 + 评分
2. **workflow 评分**：流水线式打分，管道里包含规则步骤和 LLM 评审步骤（LLM 只是评判工具，不做自主决策）

---

## 一、触发时机（三种）

| 触发点 | 动作 |
|---|---|
| message 回退（`common <= 1`） | 封口旧 session → 跑 grader → 开新 session |
| 闲置超过 2 分钟无请求 | 同上（tokio `select!` + `sleep`，每次收到 TurnRecord 重置计时器） |
| 进程退出（channel 关闭） | flush 最后一个 session，同步等 grader |

---

## 二、Session 边界检测 + 命名

复用已有 diff 算法。正常对话逐轮增长，新对话 message 数组"回缩"。

```
正常增长：
  Turn N:   [sys, user(A), assistant, tool, user(B), ...]  ← 50 msg
  Turn N+1: [sys, user(A), assistant, tool, user(B), ..., user(C)]  ← 52 msg

新对话开始：
  Turn N:   [sys, user(A), assistant, tool, user(B), ...]  ← 50 msg
  Turn N+1: [sys, user(全新问题)]                            ← 2 msg
              └─ common<=1 ─┘  整个是新 session
```

**判定**：`common_prefix_len <= 1`（只剩 system message 或无交集）

**命名**：以第 N 次回退作为 session 序号，`jsonl_ids` 直接写入 view 和 grade 文件内。

```
logs/
  session_1768000000.jsonl            ← 整个进程共用一个，长期追加

  session_1768000000_1.view.json      ← 第1次回退/超时（内含 "jsonl_ids": [2,3,4]）
  session_1768000000_1.grade.json     ← 同上

  session_1768000000_2.view.json      ← 第2次回退/超时（内含 "jsonl_ids": [7,8,9,10,11]）
  session_1768000000_2.grade.json
```

不建独立的 index.json。SessionBuilder 维护 `jsonl_ids: Vec<u64>`，`process()` 时 push，`build()` 时带上。eval/mod.rs 维护 `session_counter`，每次检测到回退/超时 +1。

---

## 三、Grader Workflow 管道

### 管道模型

```
SessionView
    │
    ▼
┌─────────────────────────────────────┐
│  Step 1: 规则统计                    │  ← 纯函数，收集定量指标
│    - 工具调用次数/成功率/重复率        │
│    - token 统计、耗时分布             │
│    - 回复文本长度、turn 数            │
│    → 产出 MetricsSnapshot            │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│  Step 2: LLM 评审                    │  ← 调 LLM API（不走代理）
│    输入：SessionView + MetricsSnapshot │
│    输出：task_completion, response_   │
│          quality 两个维度的分数+理由   │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│  Step 3: 汇总                        │  ← 纯函数
│    rules权重 + LLM权重 → overall      │
│    → GradeReport                    │
└─────────────────────────────────────┘
```

**关键设计**：grader 自己用一个 reqwest `Client` 直接调 LLM API（写死 `no_proxy()`），避免评分请求被录进 JSONL 造成套娃。

Session 检测触发时 view 立即同步落盘，grading 通过 `tokio::spawn` 后台执行，不阻塞主循环。

### 四维度划分

| 维度 | 来源 | 权重 | 说明 |
|---|---|---|---|
| task_completion | LLM | 0.35 | 是否达成用户意图、有无终态回复 |
| tool_efficiency | 规则 | 0.30 | 成功率、重复率、调用是否合理 |
| response_quality | LLM | 0.20 | 回复准确性、实质性、是否敷衍 |
| performance | 规则 | 0.15 | token 效率、耗时、turn 数量 |

---

## 四、文件结构

```
src/grader/
  mod.rs       # pub async fn run_pipeline(view, config) -> GradeReport
  rules.rs     # extract_metrics(), calc_tool_efficiency(), calc_performance()
  prompt.rs    # SessionView → 自然语言摘要 + 构造 prompt
  judge.rs     # 调 LLM API 评审（独立 reqwest Client，no_proxy）
  types.rs     # GradeReport, DimensionScore, MetricsSnapshot

src/eval/mod.rs  # Session 边界检测 + 超时 + seal_and_grade_bg()
src/main.rs      # 加载 GraderConfig，mod grader
src/config.rs    # AGENTEVAL_JUDGE_* 环境变量
src/proxy.rs     # 不改
src/format/      # 不改
```

---

## 五、配置（.env 新增）

```
AGENTEVAL_JUDGE_API_BASE=https://api.deepseek.com
AGENTEVAL_JUDGE_MODEL=deepseek-chat
AGENTEVAL_JUDGE_API_KEY=sk-xxx
```

---

## 六、输出示例

```json
{
  "session_id": "session_1768000000_1",
  "model": "MiniMax-M2.5",
  "jsonl_ids": [2, 3, 4],
  "turn_count": 3,
  "dimensions": [
    {
      "metric": "task_completion",
      "score": 0.85,
      "source": "llm",
      "reason": "agent 成功读取了 README 并向用户展示了内容，任务完成",
      "details": {},
      "weight": 0.35
    },
    {
      "metric": "tool_efficiency",
      "score": 0.90,
      "source": "rule",
      "reason": "工具调用 2/2 成功，无重复",
      "details": { "total_calls": 2, "error_count": 0, "duplicate_count": 0 },
      "weight": 0.30
    },
    {
      "metric": "response_quality",
      "score": 0.70,
      "source": "llm",
      "reason": "回复基本准确但较多冗余内容",
      "details": {},
      "weight": 0.20
    },
    {
      "metric": "performance",
      "score": 0.88,
      "source": "rule",
      "reason": "token 消耗合理，响应速度较快",
      "details": { "total_tokens_in": 15200, "total_tokens_out": 1200, "avg_duration_ms": 1800 },
      "weight": 0.15
    }
  ],
  "overall": 0.841
}
```
