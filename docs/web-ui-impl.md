# Web UI 实现记录

## 概述

在 AgentEval 代理服务中嵌入了一个 Web 评测面板，用于可视化浏览和查看会话评分结果。

## 文件变更清单

| 文件 | 操作 | 说明 |
|------|------|------|
| `src/grader/types.rs` | 修改 | `GradeReport`、`DimensionScore` 添加 `Deserialize` derive |
| `src/config.rs` | 修改 | 新增 `ui_enabled` 字段（环境变量 `AGENTEVAL_UI_ENABLED`，默认 `true`） |
| `src/proxy.rs` | 修改 | `AppState` 新增 `log_dir` 字段，供 web 路由读取日志 |
| `src/web/mod.rs` | 新增 | API 路由处理器 |
| `src/web/ui.html` | 新增 | 内嵌单页应用 UI |
| `src/main.rs` | 修改 | 注册 `mod web`，在 fallback 前添加 `/dashboard/` 路由 |

## 路由结构

```
GET /dashboard/                              → 返回 HTML 页面
GET /dashboard/api/sessions                  → 列出所有会话（JSON）
GET /dashboard/api/sessions/{session_id}     → 获取单个会话详情（JSON）
其他所有路径                                   → 代理转发（原有行为不变）
```

## 使用方式

```bash
# 启动
cargo run

# 浏览器访问
open http://127.0.0.1:57633/dashboard/
```

### 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `AGENTEVAL_UI_ENABLED` | `true` | 设为 `false` 禁用 Web UI |

## UI 功能

### Dashboard 页面

- 会话列表，按时间倒序排列
- 每行显示：Session ID、Model、Turns 数、总分（颜色编码+进度条）、四维度迷你徽标
- 总分颜色：红(<0.3) → 橙(0.3-0.5) → 黄(0.5-0.7) → 浅绿(0.7-0.85) → 绿(>0.85)
- 点击 Session ID 进入详情页
- 顶部显示总会话数、已评分数量、平均总分
- 未评分会话显示 "grading..."
- Refresh 按钮手动刷新
- Auto 开关（10 秒自动轮询 Dashboard）

### Detail 页面

- 返回 Dashboard 的链接
- 会话元信息（Model、Turns、JSONL IDs）
- 总分大数字 + 进度条
- 四维度卡片：
  - 维度名称 + 评分来源（llm / rule）
  - 分数（颜色编码数字 + 进度条）
  - 评分理由（reason）
  - 详细数据（details，JSON 格式）
  - 权重
- 对话记录（Conversation）：
  - 每个 Turn 为可折叠面板，显示耗时和 token 用量
  - User 消息以 `>` 前缀高亮
  - Step 按类型着色：紫色 `Reasoning`、蓝色 `Text`、橙色 `ToolCall`、青色 `Result`
  - ToolCall 包含工具名、参数、回填的执行结果
  - 长文本自动截断

### 边界情况处理

| 情况 | 行为 |
|------|------|
| 日志目录不存在 | 返回 `{"sessions": []}`，页面显示 "No sessions yet" |
| `.grade.json` 解析失败 | 跳过该会话，打印警告 |
| 有 `.view.json` 无 `.grade.json` | 显示为未评分，详情页显示 "Grading in progress..." |
| session_id 含 `..`、`/`、`\` | 返回 400 |
| 不存在的 session_id | 返回 404 |
| 空会话列表 | 显示 "No sessions yet" 提示 |

## 技术要点

- **零外部依赖**：HTML/CSS/JS 全部内联，通过 `include_str!` 编译进二进制文件
- **路由隔离**：`/dashboard/` 前缀与 LLM API 路径不冲突，axum 显式路由优先于 fallback
- **条件注册**：`ui_enabled = false` 时完全不注册 Web 路由，保持纯代理模式
- **会话列表实时扫描**：每次请求直接读取文件系统，无需内存索引
- **渐进增强**：评分中（尚未产出 `.grade.json`）的会话也能显示基本信息
