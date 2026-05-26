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

## 项目结构

```
AgentEval/
├── Cargo.toml          # 依赖：axum, reqwest, tokio, clap, serde_json
├── src/
│   └── main.rs         # 全部实现在一个文件（~250 行）
└── docs/
    └── implementation.md  # 你正在看的这个文件
```

## 关键依赖选型

| 组件 | 选型 | 原因 |
|---|---|---|
| HTTP server | axum 0.7 | 基于 hyper/tower，async 原生，生态成熟 |
| HTTPS 客户端 | reqwest 0.12 | 系统 TLS 支持，stream body 原生支持 |
| 异步运行时 | tokio | axum 和 reqwest 的共同运行时 |
| body 流式 | http-body-util::StreamBody | hyper 1.0 的 Frame 模型，零拷贝 chunk 转发 |
| CLI | clap 4 | Rust 社区标准 CLI 框架 |

## 边界情况

- **非 JSON body**（如二进制流、FormData）：日志记录为 `<binary N bytes>`，转发不受影响
- **上游不可达**：返回 502 Bad Gateway，错误信息写入日志
- **请求不带 stream: true**（非流式响应）：正常代理，完整响应缓冲后落盘
- **upstream 带 path 前缀**（如 `https://api.moonshot.ai/anthropic`）：`trim_end_matches('/')` 保证拼接正确
