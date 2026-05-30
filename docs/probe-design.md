# Probe Module Design — 基于 Diagnose 线索的 Agent 配置审查

## 概述

Probe 模块是一个携带文件工具的 LLM agent，进入被评测 agent 的项目目录，基于 diagnose 的症状线索，审查 prompt / skills / tools 等配置，找到行为问题的根因。

核心思路：**不调 API 做行为实验，而是直接读被评测 agent 的源码/配置做审查。**

架构上从 [agnt](https://github.com/hmbldv/agnt) 提取 Tool trait + Registry + AgentLoop 三个核心模式，零新增依赖。

### 核心原则

**1. 只读不写。** Probe agent 的 file tools 全部是只读的（read_file / grep / list_dir / glob）。不提供 write_file、edit 等写入工具。probe 是观察者和分析者，不是修改者。修改建议写在 report 的 `recommendation` 字段里，由人来决定是否执行。

**2. Diagnose 是基础探针，Probe 做发散探究。** Diagnose 用 10 条规则扫出行为症状（"哪里不对"），probe 以此为线索进入项目目录。但 probe 不应局限于 diagnose 标记的问题——在探索过程中如果发现配置的其他潜在缺陷（即使 diagnose 没标记），也应记录在 `additional_findings` 中。

**3. 证据驱动。** 每个 finding 必须附带配置文件的原文摘录（`evidence` 字段）。不凭空猜测，不凭"经验"下结论。

### 理论依据

Probe 的判断标准参考两个业界框架：

**Google Cloud 三支柱框架（2025.11）：**

| 支柱 | 评估对象 | AgentEval 对应 |
|------|---------|---------------|
| Pillar 1 — 结果质量 | 任务完成了吗？对话是否连贯？ | grader |
| Pillar 2 — 过程轨迹 | 工具选择对吗？推理逻辑对吗？效率如何？ | diagnose（症状）+ **probe（根因）** |
| Pillar 3 — 信任安全 | prompt injection 抵抗、公平性 | 暂无 |

关键洞见：**"silent failures"** — agent 可能输出了正确结果但过程是错的（比如同一个搜索重复 5 次才碰巧找到结果）。Google Pillar 2 强调必须审查过程轨迹，这正是 probe 的价值所在。

**CLEAR 框架（2025.11）：**

| 维度 | 核心发现 | 对 probe 的启发 |
|------|---------|----------------|
| **C**ost | 只看准确率会被误导——最优 agent 比性价比最优的贵 **4~10 倍** | 审查配置时关注指令冗余度，评估 token 浪费的配置根因 |
| **E**fficacy | 领域特定准确率 | 审查 skill/tool 描述是否针对实际任务调优 |
| **R**eliability | pass@k 与线上表现相关系数 0.83（单次准确率仅 0.41） | 审查配置是否有"一致性保障"（如工具调用的确定性指引） |

Google Pillar 2 的过程审查项与 diagnose 规则的对应关系：

| Google Pillar 2 指标 | Diagnose 规则（基础探针） | Probe 发散方向 |
|---------------------|------------------------|--------------|
| Tool selection accuracy | `tool_duplicate_3plus`、`tool_result_error` | 审查：工具描述是否区分了功能重叠的工具？是否有"工具选择决策树"？ |
| Input formatting accuracy | `tool_result_error`（部分） | 审查：tool 定义是否有严格的参数 schema？错误消息是否可操作？ |
| Reasoning logic / efficiency | `token_waste`、`token_excessive_input` | 审查：system prompt 是否有"已完成的任务不要重复执行"的指引？ |
| Trajectory optimality | `tool_duplicate_3plus`、`token_waste` | 审查：skill 是否有"如果 X 失败则尝试 Y"的 fallback 链？ |
| Silent failures | （diagnose 无法检测） | 发散探索：审查是否有输出验证步骤？是否有"确认任务完成"的检查点？ |

---

## 一、感知（Perception）— 输入设计

### 输入 1：DiagnoseIssue 列表

从 `.diagnose.json` 读取，每条 issue 携带：

- `category`, `severity`, `title`, `detail`（人读描述）
- `location`（jsonl_id, turn_id, step_index）
- `evidence`（相关原始数据片段）

### 输入 2：会话行为摘要

从 `.view.json` 构建紧凑摘要，不全量传入原始 JSON（防止 context 爆炸）。

构造逻辑在 `prompt.rs::format_session_summary()`：

- 每个 turn 列出 user_input（截断到 300 字符）、tool calls（名称 + 成功/失败标记）、关键文本回复
- diagnose 标记的 turn 展开更多细节
- 无 diagnose 标记的普通 turn 只保留一行摘要
- 大量正常 turn 用 `... (省略 N 个正常 turn) ...` 替代

示例格式：

```
## 会话摘要
模型: claude-sonnet-4-6
Turn 数: 5

Turn 1: 用户问 "find the bug in auth.rs"
  → agent 调用 read_file("auth.rs") ✓
  → agent 回复了代码分析

Turn 2: 用户说 "also check the login flow"
  → agent 调用 read_file("login.rs") ✓
  → agent 调用 grep("login", "src/") ✓

Turn 3:  ⚠ diagnose 标记 turn
  → agent 调用 search("auth login error handling") ✗ (无结果)
  → agent 调用 search("auth login error handling") ✗ (同上, 重复)
  → agent 调用 search("auth login error handling") ✗ (同上, 重复)

Turn 5: ⚠ diagnose 标记 turn
  → 12000 input tokens, 50 output tokens (ratio: 0.42%)
  → agent 重新 read_file("auth.rs")（turn 1 已读过）
```

### 输入 3：被评测 agent 项目目录

通过 `PROBE_SOURCE_PROJECT_DIR` 环境变量指定。Probe agent 使用 file tools 自行探索，不预设目录结构。

### 感知完整性

| 信息需求 | 来源 | 状态 |
|---------|------|------|
| 什么问题（症状） | diagnose issues | ✓ |
| 什么场景发生的 | SessionView 摘要 | ✓ |
| 被评测 agent 的配置 | 项目文件（agent 自行探索） | ✓ |
| "好"的标准是什么 | system prompt 中定义 | ✓ |
| 该 agent 的正常行为基线 | ❌ 缺失 | 首次实现不覆盖，后续可加 |

---

## 二、判断（Judgment）— 分析框架

### System Prompt 核心设计

Probe agent 的 system prompt 定义其角色和分析方法论：

```
你是 Agent 配置审查专家。你的任务是审查 agent 的
配置（system prompt / skills / tools），找到行为问题
的根因，并在探索过程中发现潜在的新问题。

重要约束：你是只读观察者。你可以读取任何文件，
但绝对不能修改、写入、删除、或执行任何命令。
所有的改进建议写入 report 的 recommendation 字段，
由开发者审查后手动执行。

## 审查方法论

对每个 diagnose issue，按以下步骤：

1. 识别配置面：这个 issue 类型通常涉及哪些配置？
   （参考下方的"诊断→配置映射表"）

2. 搜索相关文件：
   - grep 搜索工具名、skill 名、关键词
   - list_dir 了解项目结构
   - read_file 读取具体配置文件

3. 分析差距：
   - 配置中是否有应该阻止这个问题的指令？
   - 如果有但 agent 没遵守 → 指令是否被埋没/矛盾/模糊？
   - 如果没有 → 缺少什么指令？
   - 如果与配置无关 → 标记为"非配置问题"

4. 交叉验证：读取相关文件，检查是否存在矛盾

5. 发散探究：在定位到相关配置后，不要只看 diagnose
   标记的那个点。沿以下方向发散思考：
   - 同一个问题会出现在其他 skill/tool 上吗？
     如果 search 的描述有缺陷，grep 的描述会不会类似？
   - 是否有 diagnose 没检测出的 silent failure？
     比如 agent 看起来完成了任务但走了错误路径？
   - 配置之间是否有矛盾？
     比如 system prompt 说"用 grep"，但 skill 说"用 search"？
   - 是否有"几乎正确但不严谨"的指令？
     比如"搜索无结果时重试"但没有说重试上限？

6. 输出发现：每个 issue 给出根因 + 证据 + 建议 + 置信度
   发散探究中发现的新问题放入 additional_findings
```

### 诊断→配置映射表

System prompt 内建此表，指导 probe agent 的搜索方向：

| Diagnose Issue | 重点审查的配置 |
|---------------|--------------|
| `tool_duplicate_3plus` | 该 tool 的 skill 是否定义了终止条件？system prompt 是否指引了"失败后换策略"？ |
| `tool_result_error` | tool 定义的错误格式是否明确？skill 中是否说明了错误处理方式？ |
| `tool_result_missing` | tool chain 配置是否有依赖缺失？system prompt 是否有调用顺序指引？ |
| `tool_result_empty` | tool 输出规范是否有空结果说明？skill 是否区分了"成功但无输出"和"失败"？ |
| `prompt_bloat` | system prompt 全文大小，是否存在冗余指令、重复说明？ |
| `prompt_context_overflow` | 是否有上下文截断策略？对话管理配置？ |
| `token_waste` | system prompt 是否有"不要重读已读文件"的指引？skill 描述是否过度展开？ |
| `token_excessive_input` | 同上，外加是否有上下文大小相关的配置？ |
| `token_empty_response` | 流式处理配置、错误响应格式定义 |
| `view_mismatch` | 通常非配置问题，标记为数据完整性问题 |

### 置信度设计

每个 finding 带置信度：

| 置信度 | 含义 |
|--------|------|
| `high` | 找到明确配置缺陷，直接解释了 diagnose 问题 |
| `medium` | 找到可疑配置，可能相关但需进一步验证 |
| `low` | 审了配置但找不到明显原因，可能是模型行为问题 |

### 判断完整性

| 场景 | 处理方式 |
|------|---------|
| 配置明确导致问题 | 输出 high 置信度 finding + 证据摘录 |
| 配置可能相关但不明确 | 输出 medium 置信度 finding |
| 配置无关（模型行为） | 输出 low 置信度，说明"非配置问题" |
| 找不到相关文件 | finding 中注明 "could not locate config" |
| 发现 diagnose 未标记的新问题 | 放入 `additional_findings` |
| 多个 issue 同一根因 | 在 `overall_assessment` 中合并说明 |

### 输出格式

```json
{
  "findings": [
    {
      "issue_title": "tool_duplicate_3plus",
      "category": "Skill",
      "root_cause": "search skill 未定义搜索无结果时的终止条件",
      "affected_files": ["skills/search.md"],
      "confidence": "high",
      "recommendation": "在 search skill 描述中加入：如果首次搜索无结果，使用 grep 替代搜索",
      "evidence": "skills/search.md 第 5-12 行：搜索描述只说明了如何调用，未提及失败处理..."
    }
  ],
  "additional_findings": [],
  "overall_assessment": "该项目 agent 配置的主要问题是..."
}
```

---

## 三、执行（Execution）— Agent Loop

### 从 agnt 提取的三个核心组件

**Tool trait**（提取自 `agnt-core/src/tool.rs`）：

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;  // JSON Schema for OpenAI function calling
    fn call(&self, args: serde_json::Value) -> Result<String, String>;
}
```

**Registry**（提取自 `agnt-core/src/tool.rs`）：

```rust
pub struct Registry { tools: Vec<Box<dyn Tool>> }

impl Registry {
    pub fn register(&mut self, tool: Box<dyn Tool>);
    pub fn dispatch(&self, name: &str, args: Value) -> Result<String, String>;
    pub fn as_openai_tools(&self) -> Value;  // → OpenAI function calling JSON
}
```

**AgentLoop**（简化自 `agnt-core/src/agent.rs`，异步版）：

```rust
pub struct AgentLoop {
    backend: Box<dyn LlmBackend>,
    tools: Registry,
    messages: Vec<Message>,
    max_steps: usize,  // 默认 30
}

impl AgentLoop {
    pub async fn run(&mut self, user_prompt: &str) -> Result<String, String> {
        loop {
            let response = self.backend.chat(&self.messages, &self.tools.as_openai_tools()).await?;
            if response.has_tool_calls() {
                // 执行所有 tool call，结果追加到 messages
                for tc in response.tool_calls {
                    let result = self.tools.dispatch(&tc.name, tc.arguments);
                    self.messages.push(Message::tool_result(tc.id, result));
                }
            } else {
                return Ok(response.content);  // LLM 完成审查，输出 JSON
            }
        }
    }
}
```

### 四个 File Tool（全部只读）

所有工具不修改源 agent 项目的任何文件。路径安全：所有路径作为 source project dir 的相对路径 resolve，拒绝含 `..` 的路径穿越。

| Tool | 实现 | 限制 |
|------|------|------|
| `read_file` | `std::fs::read_to_string`，只读打开 | 1MB 截断 |
| `grep` | `std::process::Command("grep")` -rn，只读搜索 | 1000 行截断 |
| `list_dir` | `std::fs::read_dir`，返回 `TYPE NAME` 格式 | 无限制 |
| `glob` | `std::process::Command("find")` 带 -name 过滤 | 2000 条截断 |

**设计约束**：不提供 `write_file`、`edit`、`shell` 等写入/执行工具。Probe 的所有发现和改进建议记录在 report 的 `recommendation` 字段中，由开发者审查后手动执行。

### 执行完整性

| 场景 | 处理 |
|------|------|
| LLM 需要看某个文件 | `read_file(path)` → 返回内容 |
| LLM 需要搜索关键词 | `grep(pattern, path)` → 返回匹配行 |
| LLM 需要了解目录结构 | `list_dir(path)` → 返回文件列表 |
| LLM 需要匹配文件模式 | `glob(pattern)` → 返回匹配路径 |
| 文件太大 | 截断 + `[truncated at N bytes]` 标记 |
| 路径穿越攻击 | 拒绝 `..` + resolve 检查 |
| LLM 陷入循环 | max_steps=30 强制终止 |
| Tool 执行失败 | 返回 error string 给 LLM，LLM 自行决定如何处理 |

---

## 四、调整（Adjustment）— 迭代与反馈

### 四阶段探索策略

System prompt 内建以下策略指引：

```
Phase 1 — 概览（1-3 步）:
  list_dir / → 了解项目结构
  read_file CLAUDE.md（或 .claude/CLAUDE.md）→ 理解 agent 核心指令

Phase 2 — 定位（每个 issue 1-3 步）:
  grep 搜索 issue 中涉及的工具名/skill 名
  list_dir 浏览 skills/ tools/ prompts/ 目录
  read_file 读取找到的相关文件

Phase 3 — 深挖（按需）:
  如果发现 instructions 存在但不清晰 → grep 搜索相关关键词确认
  如果发现多个文件的 instructions 矛盾 → read_file 获取完整上下文
  如果找不到相关配置 → 记录 "could not locate config"

Phase 4 — 收束:
  当所有 issue 都有初步结论后，输出 JSON report
  不要反复读同一个文件
  不要追求完美——有合理依据即可输出 medium 置信度
```

### Loop 终止条件

1. **自然终止**：LLM 输出 JSON 报告（响应中无 tool_calls）
2. **强制终止**：达到 max_steps=30，返回 `Err("max steps exceeded")`
3. **循环检测**：同一 tool 调用 `(name + args)` 出现 3 次，注入警告消息（提取自 agnt 的 loop detection 逻辑）

### 调整完整性

| 场景 | 处理 |
|------|------|
| 正常完成 | LLM 输出 JSON → `extract_json_block()` → `ProbeReport` |
| 达到 max_steps | 返回错误，UI 提示"probe 超时，请重试" |
| LLM 读同一文件多次 | loop detection 注入警告，促使 LLM 换策略 |
| LLM 输出非纯 JSON（夹在 markdown 中） | `extract_json_block()` 降级解析（复用 grader 模式） |
| JSON 解析失败 | 保留 raw content，标记 `parse_error` |

---

## 五、模块结构与集成

### 新增文件

```
src/probe/
├── mod.rs          # pub fn run() — 读取 diagnose+view → 构造 prompt → AgentLoop → 解析 → 写 .probe.json
├── agent.rs        # AgentLoop（异步，~60 行）
├── tool.rs         # Tool trait + Registry（提取自 agnt-core）
├── tools.rs        # ReadFile, Grep, Glob, ListDir（提取自 agnt-tools，简化）
├── backend.rs      # OpenAiBackend（异步，用 reqwest）
├── prompt.rs       # build_system_prompt(), build_user_prompt(), format_session_summary()
└── types.rs        # ProbeReport, ProbeFinding, Message, ToolCall 等
```

### 修改文件

| File | Change |
|------|--------|
| `Cargo.toml` | 零新依赖 |
| `src/main.rs` | `mod probe;` + CLI `probe` subcommand + API routes |
| `src/config.rs` | `ProbeConfig` struct（环境变量：`PROBE_SOURCE_PROJECT_DIR`、`PROBE_LLM_*`） |
| `src/web/mod.rs` | `probe_session()` POST + `get_probe()` GET + `probe_summary` in `list_sessions` |
| `src/web/ui.html` | probe badge + probe panel（UI 模式与 diagnose 对称，复用 CSS 模式） |

### .env 配置

```bash
PROBE_SOURCE_PROJECT_DIR=/path/to/agent/project   # 被评测 agent 的项目根目录
PROBE_LLM_API_BASE=https://api.openai.com/v1       # LLM API 地址
PROBE_LLM_MODEL=claude-sonnet-4-6                   # 用于审查的模型
PROBE_LLM_API_KEY=sk-xxx                            # API Key
```

### CLI

```bash
cargo run -- probe <session_id>
```

终端输出：每一步 tool call 的名称和结果摘要，最终输出 ProbeReport JSON。

### API

```
POST /dashboard/api/sessions/{session_id}/probe   # 执行 probe（同步返回）
GET  /dashboard/api/sessions/{session_id}/probe   # 读取已有 .probe.json
```

### 数据流

```
┌─────────────┐     ┌──────────────┐     ┌──────────────────┐
│ .diagnose   │     │ .view.json   │     │ .env             │
│ .json       │     │              │     │ PROBE_SOURCE_    │
│ issues[]    │     │ turns[]      │     │ PROJECT_DIR      │
└──────┬──────┘     └──────┬───────┘     └────────┬─────────┘
       │                   │                      │
       └───────────────────┼──────────────────────┘
                           │
                           ▼
              build_user_prompt()
              ├── diagnose issues (JSON)
              └── session summary (compact text)
                           │
                           ▼
              ┌─────────────────────────┐
              │     AgentLoop.run()     │
              │                         │
              │  system: "你是 Agent    │
              │  配置审查专家..."       │
              │                         │
              │  tools:                 │
              │   read_file             │
              │   grep                  │
              │   list_dir              │
              │   glob                  │
              │                         │
              │  loop:                  │
              │   LLM 规划探索步骤      │
              │   → 调用 file tools    │
              │   → 分析结果            │
              │   → 深入或收束          │
              └───────────┬─────────────┘
                          │
                          ▼
              LLM 输出 JSON report
                          │
                          ▼
              extract_json_block()
              → ProbeReport
                          │
                          ▼
              write .probe.json
              → logs/<session_id>.probe.json
```

### 持久化

```
logs/
  session_1779871837_1.view.json
  session_1779871837_1.grade.json
  session_1779871837_1.diagnose.json
  session_1779871837_1.probe.json    ← NEW
```

---

## 六、与 diagnose / grader 的关系

```
                 ┌──────────┐
                 │  proxy   │ ← 记录原始 API 流量
                 └────┬─────┘
                      │
          ┌───────────┼───────────┐
          ▼           ▼           ▼
    ┌─────────┐ ┌──────────┐ ┌──────────┐
    │  eval   │ │ diagnose │ │  grader  │
    │ 构建    │ │ 规则引擎 │ │ LLM 评分 │
    │ Session │ │ 找症状   │ │ 给分数   │
    │ View    │ │          │ │          │
    └────┬────┘ └────┬─────┘ └──────────┘
         │           │
         │    issues: [tool_duplicate_3plus, ...]
         │           │
         ▼           ▼
    ┌─────────────────────────┐
    │        probe            │
    │  LLM agent + file tools│
    │  审查被评测 agent 配置  │
    │  找根因 + 给改进建议    │
    └────────────┬────────────┘
                 │
                 ▼
           ProbeReport
           根因 + 建议 + 证据
```

| | diagnose | grader | probe |
|---|---|---|---|
| **回答什么** | 哪里有异常 | 打几分 | 为什么异常、怎么改 |
| **方法** | 10 条纯规则 | LLM 评分 | LLM agent + file tools |
| **数据来源** | view.json + JSONL | SessionView | 被评测 agent 的项目文件 |
| **输出** | .diagnose.json | .grade.json | .probe.json |
| **定位** | 法医验尸（读 log） | 裁判打分 | 活体审查（读配置源码） |

---

## 七、验证

1. `cargo build` 零警告
2. `.env` 设置 `PROBE_SOURCE_PROJECT_DIR` 指向一个含 CLAUDE.md + skills 的 agent 项目
3. `cargo run -- diagnose <session_id>` 先跑 diagnose
4. `cargo run -- probe <session_id>` → 终端可观察 tool calls 执行过程 → 输出结构化 ProbeReport JSON
5. 检查 `logs/<session_id>.probe.json` 包含有效 findings（每条含 root_cause + evidence + recommendation + confidence）
6. Dashboard：probe badge 正确展示状态，detail view 有 probe panel
