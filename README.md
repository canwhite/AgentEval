# AgentEval

A transparent HTTP proxy that captures Agent ↔ LLM API traffic, auto-splits sessions, builds structured conversation views, and grades each session with a multi-dimensional scoring pipeline — **now with a built-in web dashboard.**

*透明 HTTP 代理，捕获 Agent ↔ LLM 的 API 流量，自动切分 session、构建结构化视图、多维自动评分 —— 内置 Web 评测面板。*

## How It Works / 工作原理

```
Agent ── HTTP ──▶ AgentEval (127.0.0.1:57633) ──▶ LLM API
                       │
                       ├─ Raw traffic → logs/{timestamp}.jsonl
                       │   原始流量记录
                       ├─ Session boundary detection (message rollback / idle timeout)
                       │   实时检测 session 边界（消息回退 / 闲置超时）
                       ├─ Structured views → logs/{session}_N.view.json
                       │   结构化会话视图
                       └─ Auto-grade (rules + LLM) → logs/{session}_N.grade.json
                          + Web dashboard → http://127.0.0.1:57633/dashboard/
                          自动评分 + Web 面板查看
```

## Quick Start / 快速开始

### 1. Configure `.env` / 配置

```bash
# Upstream LLM API / 上游 LLM 地址
AGENTEVAL_UPSTREAM=https://api.edgefn.net

# Proxy port / 代理监听端口
AGENTEVAL_PORT=57633

# Log directory / 日志目录
AGENTEVAL_LOG_DIR=./logs

# Judge LLM for auto-grading / 评测 LLM
AGENTEVAL_JUDGE_API_BASE=https://api.deepseek.com
AGENTEVAL_JUDGE_MODEL=deepseek-chat
AGENTEVAL_JUDGE_API_KEY=sk-xxx

# Optional: disable web UI / 可选：禁用 Web UI
# AGENTEVAL_UI_ENABLED=false
```

### 2. Start the proxy / 启动代理

```bash
cargo run
# listening http://127.0.0.1:57633 -> https://api.edgefn.net
# dashboard http://127.0.0.1:57633/dashboard/
```

### 3. Configure your Agent / 配置 Agent

Point your Agent's `BASE_URL` to the proxy. No code changes needed.

*将 Agent 的 `BASE_URL` 指向代理地址，无需任何改动。*

```bash
export OPENAI_BASE_URL=http://127.0.0.1:57633/v1
# or / 或
MODEL_BASE_URL=http://127.0.0.1:57633/v1 your-agent-command
```

### 4. View results / 查看结果

Open **http://127.0.0.1:57633/dashboard/** in your browser.

*浏览器打开上述地址即可查看评测面板。*

## Web Dashboard / Web 面板

The dashboard displays all evaluated sessions with scores and conversation details.

*面板展示所有已评测会话，包含评分和对话详情。*

| Feature / 功能 | Description / 说明 |
|---|---|
| Session list / 会话列表 | All sessions sorted by time, with overall score + dimension mini-bars / 按时间排列，显示总分+四维迷你进度条 |
| Detail view / 详情页 | Full dimension breakdown with LLM judge reasons + conversation transcript / 完整维度得分+评分理由+对话记录 |
| Manual grading / 手动评分 | Click **Grade** on ungraded sessions to trigger evaluation / 点击未评分会话的 **Grade** 按钮手动触发 |
| Auto-refresh / 自动刷新 | Toggle to poll every 10s / 开关自动刷新，10 秒轮询 |
| Score colors / 分数着色 | Red(<0.3) → Orange(0.3-0.5) → Yellow(0.5-0.7) → Green(>0.7) / 红→橙→黄→绿 |

## Session Splitting / Session 自动切分

The proxy detects conversation boundaries automatically within a single process:

*代理在单进程内自动检测对话边界：*

| Trigger / 触发条件 | Behavior / 行为 |
|---|---|
| New conversation (message array rollback) / 用户开新对话 | Seal old session → background grade → start new session / 封口旧 session → 后台评分 → 开新 session |
| 2-minute idle timeout / 2 分钟无新请求 | Same as above / 同上 |
| Proxy shutdown / 进程退出 | Flush last session (synchronous grade) / Flush 最后一个 session（同步等评分） |

**Detection logic / 判定逻辑:** Normal conversations grow messages turn-by-turn. A new conversation "shrinks" back to just the system prompt + new question. When `common_prefix_len <= 1`, it's treated as a new session.

*正常对话 messages 逐轮增长，新对话 messages 会"回缩"。当 `common_prefix_len <= 1` 时判定为新 session。*

## Output Files / 输出文件

```
logs/
  session_1768000000.jsonl             ← All raw requests in this process / 进程内所有原始请求
  session_1768000000_1.view.json       ← Session 1 structured view / 第 1 个会话结构化视图
  session_1768000000_1.grade.json      ← Session 1 grade report / 第 1 个会话评分报告
  session_1768000000_2.view.json       ← Session 2
  session_1768000000_2.grade.json
  ...
```

### `.view.json` — Structured session view / 结构化视图

Contains turn sequence, tool call pairing (cross-turn backfill), user input, token usage.

*包含 turn 序列、tool call 跨 turn 配对、user input、token 用量。*

### `.grade.json` — Auto-grade report / 自动评分报告

Four weighted dimensions summed to an overall 0–1 score:

*四个加权维度，汇总为 0-1 总分：*

| Dimension / 维度 | Source / 来源 | Weight / 权重 | What it measures / 衡量内容 |
|---|---|---|---|
| `task_completion` | LLM | 0.35 | Whether the user's intent was fulfilled / 是否达成用户意图 |
| `tool_efficiency` | Rule | 0.30 | Tool call success rate, duplicate penalty / 工具调用成功率、重复惩罚 |
| `response_quality` | LLM | 0.20 | Accuracy, conciseness, substance / 回复准确性、简洁性、实质性 |
| `performance` | Rule | 0.15 | Token ratio, latency, turn count / Token 效率、耗时、turn 数 |

**Pipeline / 流水线:** Rule-based metrics → LLM judge → fallback heuristics (when LLM is unavailable).

*规则统计 → LLM 评审 → 降级兜底（LLM 不可用时自动切换）。*

## Configuration Reference / 配置参考

| Variable / 变量 | Default / 默认值 | Description / 说明 |
|---|---|---|
| `AGENTEVAL_UPSTREAM` | `https://api.deepseek.com` | Target LLM API / 上游 API 地址 |
| `AGENTEVAL_PORT` | `57633` | Local proxy port / 代理端口 |
| `AGENTEVAL_LOG_DIR` | `~/.agenteval/logs` | Log output directory / 日志目录 |
| `AGENTEVAL_VERBOSE` | `false` | Print request bodies / 打印请求体 |
| `AGENTEVAL_UI_ENABLED` | `true` | Enable web dashboard / 启用 Web 面板 |
| `AGENTEVAL_JUDGE_API_BASE` | same as upstream / 同上 | Judge LLM URL / 评测 LLM 地址 |
| `AGENTEVAL_JUDGE_MODEL` | `MiniMax-M2.5` | Judge LLM model / 评测 LLM 模型 |
| `AGENTEVAL_JUDGE_API_KEY` | (empty) | Judge LLM API key / 评测 LLM API Key |

## Docs / 文档

- [Web UI Design Plan / Web UI 设计计划](docs/web-ui-plan.md)
- [Web UI Implementation / Web UI 实现记录](docs/web-ui-impl.md)
- [Grader Design / Grader 方案设计](docs/grader-design.md)
- [Grader Implementation / Grader 实现细节](docs/grader-impl.md)
- [Eval Module Implementation / Eval 模块实现](docs/eval-impl.md)
- [Data Flow / 数据流](docs/dataflow.md)
- [Proxy Architecture / 代理架构](docs/proxy.md)
