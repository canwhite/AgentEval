# AgentEval

透明 HTTP 代理，捕获 Agent ↔ LLM 的 API 流量，自动分 session、结构化、评分。

## 工作原理

```
Agent 代码 ── HTTP ──▶ AgentEval (127.0.0.1:57633) ──▶ LLM API
                           │
                           ├─ 原始流量 → logs/{timestamp}.jsonl
                           ├─ 实时检测 session 边界（消息回退 / 闲置超时）
                           ├─ 构建结构化视图 → logs/{session}_N.view.json
                           └─ 自动评分（规则 + LLM）→ logs/{session}_N.grade.json
```

## 快速开始

### 1. 配置 .env

```bash
# 上游 LLM API 地址
AGENTEVAL_UPSTREAM=https://api.edgefn.net

# 代理监听端口
AGENTEVAL_PORT=57633

# 日志目录
AGENTEVAL_LOG_DIR=./logs

# 评测 LLM 配置（用于自动评分）
AGENTEVAL_JUDGE_API_BASE=https://api.deepseek.com
AGENTEVAL_JUDGE_MODEL=deepseek-chat
AGENTEVAL_JUDGE_API_KEY=sk-xxx
```

### 2. 启动代理

```bash
cargo run
# listening http://127.0.0.1:57633 -> https://api.edgefn.net
```

### 3. 启动你的 Agent

将 Agent 的 `MODEL_BASE_URL` 指向代理地址：

```bash
MODEL_BASE_URL=http://127.0.0.1:57633/v1 bun run server/index.ts
```

Agent 无需任何改动，所有对 LLM 的请求都会透明经过代理。

## Session 自动切分

代理在一个进程内自动检测对话边界，无需重启：

| 触发条件 | 行为 |
|---|---|
| 用户开新对话（message 数组回退） | 封口旧 session → 后台评分 → 开新 session |
| 2 分钟无新请求 | 同上 |
| 代理进程退出 | Flush 最后一个 session（同步等评分完成） |

### 判定逻辑

正常对话 messages 逐轮增长，新对话 messages 会"回缩"（只剩 system prompt + 新问题）。当 `common_prefix_len <= 1` 时判定为新 session。

## 输出文件

```
logs/
  session_1768000000.jsonl             ← 进程内所有原始请求/响应
  session_1768000000_1.view.json       ← 第 1 个 session 的结构化视图
  session_1768000000_1.grade.json      ← 第 1 个 session 的评分结果
  session_1768000000_2.view.json       ← 第 2 个 session
  session_1768000000_2.grade.json
  ...
```

### .view.json

结构化后的会话视图，包含 turn 序列、tool call 配对、user input、token 用量等。

### .grade.json

自动评分结果，四个维度加权汇总：

| 维度 | 来源 | 权重 | 说明 |
|---|---|---|---|
| `task_completion` | LLM | 0.35 | 是否达成用户意图 |
| `tool_efficiency` | 规则 | 0.30 | 工具调用成功率、重复率 |
| `response_quality` | LLM | 0.20 | 回复准确性、实质性 |
| `performance` | 规则 | 0.15 | token 效率、耗时、turn 数 |

评分流水线：规则统计 → LLM 评审 → 降级兜底（LLM 不可用时自动切换规则推算）。

## 文档

- [Grader 方案设计](docs/grader-design.md)
- [Grader 实现细节](docs/grader-impl.md)
- [Eval 模块实现](docs/eval-impl.md)
