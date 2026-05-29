# Web UI 设计计划

## 背景

AgentEval 是一个 Rust/axum HTTP 代理，用于捕获 AI Agent → LLM 的请求流量，构建结构化会话视图，并在 4 个维度上自动评分。当前所有输出均为文件形式（`.view.json` / `.grade.json` 存放在日志目录中），**没有 UI** —— 用户必须手动阅读 JSON 文件。需要为评测条目提供可视化界面。

## 方案

**嵌入式单页 HTML + 原生 JS**，通过 `include_str!` 编译进 axum 二进制文件。无 npm、无构建工具、无外部 CDN。新增 2 个文件，修改 4 个现有文件。UI 访问地址：`http://127.0.0.1:57633/dashboard/`

## 新增文件

### `src/web/mod.rs` — API 路由处理器

- `serve_ui()` → 返回内嵌的 HTML 页面
- `list_sessions()` → 扫描 `log_dir` 中的 `.view.json` / `.grade.json` 文件，按时间倒序返回会话摘要列表
- `get_session(Path(session_id))` → 返回组合后的 `{ view: SessionView, grade: GradeReport | null }`
- `is_safe_session_id()` → 拒绝路径穿越字符（`..`、`/`、`\`）

### `src/web/ui.html` — 内嵌单页应用

- 暗色主题，内联 CSS，原生 JavaScript
- **Dashboard 视图**（默认）：会话表格，显示 session_id、model、turns、总分（颜色编码+进度条）、4 维度迷你进度条
- **Detail 视图**（`?view=detail&session=ID`）：完整维度得分分解（含评分理由），会话对话记录（turns → steps 附类型徽标）
- 分数进度条：宽度按比例的水平色条（红 <0.3 / 橙 0.3-0.5 / 黄 0.5-0.7 / 浅绿 0.7-0.85 / 绿 >0.85）
- Step 类型徽标：`[Reasoning]` 紫色、`[Text]` 蓝色、`[ToolCall]` 橙色、`[Result]` 青色
- 刷新按钮 + 自动刷新开关（10 秒轮询）
- 客户端路由通过 `URLSearchParams` + `history.pushState` 实现

## 修改的现有文件

### `src/grader/types.rs`
- 为 `GradeReport` 和 `DimensionScore` 添加 `#[derive(Deserialize)]`

### `src/proxy.rs` — AppState
- 新增 `pub log_dir: String` 字段
- 在 `AppState::new()` 中从 `config.log_dir` 赋值

### `src/config.rs`
- 新增 `pub ui_enabled: bool` 字段，默认 `true`，通过环境变量 `AGENTEVAL_UI_ENABLED` 配置

### `src/main.rs`
- 添加 `mod web;`
- 在代理 fallback 之前注册 Web 路由：
  - `GET /dashboard/` → `web::serve_ui`
  - `GET /dashboard/api/sessions` → `web::list_sessions`
  - `GET /dashboard/api/sessions/{session_id}` → `web::get_session`

## 路由设计

`/dashboard/` 前缀不会与 LLM API 路径（`/v1/chat/completions` 等）冲突。axum 在 fallback 之前先匹配显式路由，因此代理流量不受影响。

## 无新增依赖

所有需要的 crate（axum、serde、serde_json、tokio、reqwest）已在 `Cargo.toml` 中。

## API 设计

### `GET /dashboard/api/sessions`

```json
{
  "sessions": [
    {
      "session_id": "session_1779871837_1",
      "model": "MiniMax-M2.5",
      "turn_count": 3,
      "jsonl_ids": [1, 2, 3],
      "overall": 0.90,
      "graded": true,
      "dimensions": [
        { "metric": "task_completion", "score": 0.80, "source": "llm", "weight": 0.35 },
        { "metric": "tool_efficiency", "score": 1.0,  "source": "rule", "weight": 0.30 },
        { "metric": "response_quality", "score": 0.90, "source": "llm", "weight": 0.20 },
        { "metric": "performance", "score": 0.92, "source": "rule", "weight": 0.15 }
      ]
    }
  ]
}
```

### `GET /dashboard/api/sessions/{session_id}`

```json
{
  "view": { /* SessionView 完整结构 */ },
  "grade": { /* GradeReport 或 null */ }
}
```

## 实现顺序

1. 为 `grader/types.rs` 中的 `GradeReport` + `DimensionScore` 添加 `Deserialize`
2. 在 `proxy.rs` 的 `AppState` 中增加 `log_dir`，在 `config.rs` 中增加 `ui_enabled`
3. 创建 `src/web/mod.rs` — 路由处理器
4. 创建 `src/web/ui.html` — 完整的单页 UI
5. 在 `main.rs` 中注册路由（添加 `mod web;`，在 fallback 之前注册路由）
6. 验证：`cargo check` / `cargo build`

## 验证方法

1. `cargo build` 通过
2. 启动 `cargo run`，浏览器打开 `http://127.0.0.1:57633/dashboard/`
3. Dashboard 显示 `logs/` 目录中的已有会话
4. 点击会话 → 详情页显示 4 维度评分及对话记录
5. 通过代理发送 Agent 请求，刷新后新会话出现
6. 正常代理流量（`/v1/chat/completions`）通过 `curl` 验证正常转发
