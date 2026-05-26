# agenteval 实现细节

## 一句话

**不劫持 TLS，劫持 URL。** 让客户端以为 `localhost:57633` 就是 API 服务器，把它发出的请求引到本地代理，由代理转发到真实上游。

---

## 流量转化全流程

这是整个系统最核心的链路，每一步都精确描述流量是如何被"偏转"的：

### 第 0 步：你设置了什么

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:57633
```

一行为什么能抓到所有包？

Claude Code、OpenAI SDK、langchain 等客户端，内部都用一个 HTTP client，向 `BASE_URL` 发请求。正常情况：

```
客户端 ── HTTPS ──► api.anthropic.com:443
```

你把 `BASE_URL` 改成 `http://127.0.0.1:57633` 后：

```
客户端 ── HTTP ──► 127.0.0.1:57633
```

客户端是"被骗"的一方 —— 它根本不知道对面不是真正的 API 服务器。它老老实实地把自己的请求（包括 Authorization header、JSON body）用 HTTP 明文发给本地的 57633 端口。

### 第 1 步：请求到达代理

**代理是一个 axum HTTP server，只绑定 127.0.0.1，不对外暴露。**

```
客户端                     agenteval (127.0.0.1:57633)
  │                              │
  │  POST /v1/messages           │
  │  Host: 127.0.0.1:57633      │
  │  Authorization: sk-ant-...   │
  │  Content-Type: application/json
  │  {"model":"claude-4.6",...}  │
  │ ──────────────────────────►  │
  │                              │
```

axum 的 `Router::fallback(any(proxy_handler))` 接收**所有** HTTP 方法（GET/POST/PATCH/...）、**所有**路径（`/v1/messages`、`/v1/models`、`/`）。

### 第 2 步：拼接上游 URL

```rust
// proxy_handler 中
let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
let upstream_url = format!("{}{}", state.upstream_base, path_and_query);
```

**关键：只有 path + query 被拼接，Host 被替换。** 这是流量转移的本质。

一个具体例子：

| 客户端请求 | upstream 配置 | 拼接结果 |
|---|---|---|
| `http://127.0.0.1:57633/v1/messages` | `https://api.anthropic.com` | `https://api.anthropic.com/v1/messages` |
| `http://127.0.0.1:57633/v1/messages?beta=true` | `https://api.openai.com` | `https://api.openai.com/v1/messages?beta=true` |
| `http://127.0.0.1:8080/` | `https://httpbin.org` | `https://httpbin.org/` |

原始的 `Host: 127.0.0.1:57633` 被**丢弃**，替换为上游的 host。客户端以为自己发给了本地，实际上请求的 path 和 body 被完整打包，发往了真正的 API。

### 第 3 步：Header 清洗与转发

```
客户端 ──► proxy                                    upstream ◄── proxy
          │                                                    │
          │  Host: 127.0.0.1:57633       ── 丢弃               │
          │  Content-Length: 1234         ── 丢弃（reqwest 自算）│
          │  Transfer-Encoding: chunked   ── 丢弃               │
          │  Authorization: sk-ant-...    ── 原样转发 ────────►  │
          │  Content-Type: application/json── 原样转发 ────────►  │
          │  anthropic-version: 2023-06-01 ── 原样转发 ────────►  │
          │  x-api-key: sk-...            ── 原样转发 ────────►  │
```

```rust
for (key, value) in headers.iter() {
    let k = key.as_str().to_lowercase();
    if k == "host" || k == "content-length" || k == "transfer-encoding" {
        continue; // 跳过这 3 个
    }
    req_builder = req_builder.header(key.as_str(), value);
}
```

**为什么跳过这三个？**

- **`Host`**：客户端写的是 `127.0.0.1:57633`，但我们要发给 `api.anthropic.com`。reqwest 会自动根据目标 URL 填入正确的 Host header 和 SNI。
- **`Content-Length` / `Transfer-Encoding`**：reqwest 会根据实际 body 大小重新计算。原样转发会导致长度不匹配。

### 第 4 步：代理发出 HTTPS 请求

```rust
state.client.request(method, &upstream_url)  // reqwest Client
    .headers(cleaned_headers)
    .body(body_bytes)
    .send()
```

**这里是流量从"明文"变回"加密"的地方。**

reqwest 是一个完整的 HTTPS 客户端。当它看到 `upstream_url = "https://api.anthropic.com/v1/messages"` 时：

1. 通过系统 DNS 解析 `api.anthropic.com` 的真实 IP
2. 与真实 IP 的 443 端口建立 TCP 连接
3. 执行标准 TLS 1.3 握手（验证证书链、SNI）
4. 在加密通道中发送 HTTP 请求

所以从上游 API 的视角，这个请求和客户端直接发出的**没有任何区别**。它就是一次正常的 HTTPS 请求。

```
                        agenteval 内部
客户端 ◄── HTTP ──► [axum server] ── 内存 ──► [reqwest client] ◄── HTTPS ──► api.anthropic.com
                          │                       │
                    plaintext buffer          TLS 1.3 + 证书验证
                    127.0.0.1:57633          真实 IP:443
```

### 第 5 步：响应流式透传

上游的 HTTPS 响应到达后，代理把它拆成字节流，逐个 chunk 推回给客户端：

```
api.anthropic.com                  agenteval                      客户端
      │                              │                              │
      │  HTTPS response              │                              │
      │  (SSE: data: {"delta":      │                              │
      │   {"text":"Hello"}} )        │                              │
      │ ──────────────────────────►  │                              │
      │                              │  HTTP response               │
      │                              │  (same bytes, no TLS)        │
      │                              │ ──────────────────────────►  │
      │                              │                              │
      │  SSE: data: {"delta":       │                              │
      │  {"text":" world"}} )        │                              │
      │ ──────────────────────────►  │                              │
      │                              │  SSE: data: {"delta":       │
      │                              │  {"text":" world"}} )        │
      │                              │ ──────────────────────────►  │
```

流式透传的实现：

```rust
let frame_stream = upstream_resp.bytes_stream().map(|result| {
    match result {
        Ok(bytes) => Ok(Frame::data(bytes)),
        Err(e) => Err(Box::new(e) as ...),
    }
});
let axum_body = Body::new(StreamBody::new(frame_stream));
```

- `reqwest::Response::bytes_stream()` 返回一个异步流，每收到一个 TCP segment 就 yield 一个 `Bytes` chunk
- `Frame::data(bytes)` 把裸字节包装成 HTTP body frame
- `StreamBody` 把 frame 流转成 axum 可发送的 response body
- **不做 buffering** —— 上游发一段，代理就转发一段。客户端不需要等整个响应完成，可以看到 token-by-token 的实时输出

### 第 6 步：写入审计日志

每个请求完成后，代理将完整记录写入 `~/.agenteval/logs/0001.json`：

```json
{
  "id": 1,
  "ts": 1779805904142,
  "method": "POST",
  "path": "/v1/messages",
  "upstream": "https://api.anthropic.com/v1/messages",
  "request_headers": { "authorization": "sk-ant-...", "content-type": "application/json" },
  "request_body": { "model": "claude-4.6", "messages": [...] },
  "response_status": 200,
  "response_headers": { "content-type": "text/event-stream", ... },
  "duration_ms": 3421,
  "streaming": true
}
```

---

## 为什么这个方案能工作，而传统方案不行

| 传统 MITM 方案 | agenteval |
|---|---|
| 代理伪装成 API 服务器，**自己签发 TLS 证书**，需要系统信任该 CA | 代理是**纯 HTTP server**，TLS 由 reqwest 在出站侧处理，使用系统认可的证书 |
| 依赖 `HTTP_PROXY` / `HTTPS_PROXY` 环境变量 | 利用各 CLI 的 `BASE_URL` 环境变量 |
| CLI 工具通常不读 `HTTP_PROXY`，直接无代理连接 | CLI 总是读 `BASE_URL`，这是 SDK 级别的参数 |
| 证书绑定（certificate pinning）直接失效 | 客户端看到的是真实 API 的证书，绑定逻辑正常 |
| 每次原始报文，人工解析 | 按 API 格式结构化存储 request body |

**核心优势：客户端自己完成 TLS 连接的事实检查（证书链、SNI、OCSP）**。代理只做它擅长的事 —— 在中间那个唯一的明文 hop 上坐下来，不声不响地记录一切。

---

## 怎么用

### 1. 配置

三种方式，自由混用。优先级：**CLI 参数 > 环境变量 > 默认值**。

**方式 A：`.env` 文件（无需每次敲参数）**

```bash
cp .env.example .env
```

```bash
# .env
AGENTEVAL_UPSTREAM=https://api.anthropic.com
AGENTEVAL_PORT=57633
# AGENTEVAL_VERBOSE=1
```

**方式 B：环境变量（shell export）**

```bash
export AGENTEVAL_UPSTREAM=https://api.openai.com
```

**方式 C：CLI 参数（一次性覆盖）**

```bash
cargo run -- --upstream https://api.openai.com --port 8080 --verbose
```

完整的参数列表：

```
$ cargo run -- --help

Usage: AgentEval [OPTIONS]

Options:
  -u, --upstream <UPSTREAM>  上游 API 地址    [env: AGENTEVAL_UPSTREAM=]  [default: https://api.anthropic.com]
  -p, --port <PORT>          本地监听端口      [env: AGENTEVAL_PORT=]      [default: 57633]
      --log-dir <LOG_DIR>    日志存放目录      [env: AGENTEVAL_LOG_DIR=]   [default: ~/.agenteval/logs]
  -v, --verbose              详细模式          [env: AGENTEVAL_VERBOSE=]
  -h, --help                 打印帮助
```

### 2. 常见组合

```bash
# 日常使用：.env 配好 upstream，直接跑
cargo run

# 临时抓 OpenAI：CLI 覆盖 upstream
cargo run -- --upstream https://api.openai.com

# 抓 Kimi（带路径前缀）
cargo run -- --upstream https://api.moonshot.cn/anthropic

# 详细模式：打印每个请求的 JSON body
cargo run -- --verbose

# 自定义端口 + 详细模式
cargo run -- --port 6789 --verbose

# 纯环境变量（不依赖 .env）
AGENTEVAL_UPSTREAM=https://api.deepseek.com cargo run
```

### 3. 让客户端"认错门"

```bash
# Claude Code
ANTHROPIC_BASE_URL=http://127.0.0.1:57633 claude

# OpenAI / Codex
OPENAI_BASE_URL=http://127.0.0.1:57633 codex

# 或 export 到当前 shell
export ANTHROPIC_BASE_URL=http://127.0.0.1:57633
claude
```

客户端把所有请求发给 `127.0.0.1:57633`，代理再转发到配置的上游。

### 4. 看日志

```bash
ls ~/.agenteval/logs/
# 0001.json  0002.json  0003.json  ...

cat ~/.agenteval/logs/0001.json | python3 -m json.tool
```

每条记录结构见上方「第 6 步：写入审计日志」。

---

## 项目结构

```
AgentEval/
├── .env.example         # 环境变量模板（提交 git）
├── .env                 # 你的本地配置（gitignore）
├── Cargo.toml           # 依赖：axum, reqwest, tokio, serde_json, dotenvy, clap
├── src/
│   ├── main.rs          # 入口：加载 .env，启动 server（~30 行）
│   ├── config.rs        # Config::load()，读环境变量
│   └── proxy.rs         # proxy::handler() + 辅助函数
└── docs/
    └── proxy.md         # 你正在看的这个文件
```

| 文件 | 职责 |
|---|---|
| `config.rs` | `Config::load()` → `dotenvy::dotenv()` + `clap::Parser`，从 `.env` / 环境变量 / CLI 读配置 |
| `proxy.rs` | `proxy::handler()` → 接收请求→清洗 header→转发上游→流式透传→写日志 |
| `main.rs` | `main()` → `Config::load()` + `axum::serve()` |

## 关键依赖选型

| 组件 | 选型 | 原因 |
|---|---|---|
| HTTP server | axum 0.7 | 基于 hyper/tower，async 原生，生态成熟 |
| HTTPS 客户端 | reqwest 0.12 | 系统 TLS 支持，stream body 原生支持 |
| 异步运行时 | tokio | axum 和 reqwest 的共同运行时 |
| body 流式 | http-body-util::StreamBody | hyper 1.0 的 Frame 模型，零拷贝 chunk 转发 |
| 配置 | dotenvy + clap | dotenvy 加载 `.env` → 环境变量 → clap 自动绑定，CLI 优先 |

## 配置优先级

```
CLI 参数 > 环境变量 (AGENTEVAL_*) > .env 文件 > 默认值
    │              │                      │           │
    │   cargo run ─│─ --upstream https://x.com --verbose
    │              │                      │           │
    │   export AGENTEVAL_UPSTREAM=https://x.com      │
    │              │                      │           │
    │              │    .env 中写的值      │           │
    │              │                      │           │
    │              │              代码中的 default_value_t
```

clap 的 `#[arg(env = "...")]` 让每个参数自动从同名环境变量取值，dotenvy 在 clap 解析前把 `.env` 注入到进程环境。所以 `.env` 和环境变量对 clap 来说是一回事，后者覆盖前者。

## 边界情况

- **非 JSON body**（如二进制流、FormData）：日志记录为 `<binary N bytes>`，转发不受影响
- **上游不可达**：返回 502 Bad Gateway，错误信息写入日志
- **请求不带 stream: true**（非流式响应）：正常代理，完整响应缓冲后落盘
- **upstream 带 path 前缀**（如 `https://api.moonshot.ai/anthropic`）：`trim_end_matches('/')` 保证拼接正确
