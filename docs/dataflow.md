# AgentEval 数据流转

一个请求从客户端发出到被代理转发、记录、评分的完整路径。

## 请求侧：客户端 → 代理 → 上游

```
1. 客户端发请求
   POST /v1/chat/completions
   {"model":"gpt-4","messages":[...],"stream":true}
   Host: 127.0.0.1:57633
   Authorization: Bearer sk-xxx
   Content-Type: application/json
        │
        ▼
2. axum 接收，拆成四块
   method:  POST
   uri:     /v1/chat/completions
   headers: {host, authorization, content-type, content-length, ...}
   body:    [字节数组]
        │
        ▼
3. 拼接上游 URL
   path_and_query = "/v1/chat/completions"
   upstream_url   = "https://api.edgefn.net/v1/chat/completions"
        │
        ▼
4. 清洗 header
   扔掉: host, content-length, transfer-encoding
   保留: authorization, content-type, 及所有自定义 header
        │
        ▼
5. reqwest 转发到上游
   HTTPS POST https://api.edgefn.net/v1/chat/completions
   reqwest / hyper 自动补上:
     Host: api.edgefn.net          ← 从 upstream_url 提取
     Content-Length: <真实长度>     ← 从 body.len() 计算
```

## 响应侧：上游 → 代理 → 客户端（实时）+ 后台（写日志）

```
6. 上游返回流式响应
   HTTP/1.1 200 OK
   content-type: text/event-stream
   chunk0 chunk1 chunk2 chunk3 ...
        │
        ▼
7. upstream_resp.bytes_stream()
   → Stream<Item=Result<Bytes, Error>>
   每收到一个 TCP segment 就 yield 一个 Bytes chunk
        │
        ▼
8. .map() 拆流 → Frame 通道 + channel tee
   
   chunk0 ──┬──▶ Frame::data(chunk0) ──┐
            │                          │
            └──▶ tx.send(chunk0) ────┐ │
                                     │ │
   chunk1 ──┬──▶ Frame::data(chunk1)─┤ │
            │                        │ │
            └──▶ tx.send(chunk1) ──┐ │ │
                                   │ │ │
   chunk2  ...                    ... │ │
                                     ▼ ▼
                               StreamBody
                                     │
                                     ▼
                                 axum::Body
                                     │
                                     ▼
   Response {                         
     status:  200,                    
     headers: { content-type: ... },  
     body:    StreamBody              
   }
        │
        ▼
9. 推给客户端（实时，每个 chunk 一到就走）
   HTTP/1.1 200 OK
   content-type: text/event-stream
   chunk0 chunk1 chunk2 chunk3 ...
```

## 后台侧：拼完整 body → 写 JSONL → 发 eval → 评分

```
后台 task (tokio::spawn)

   rx 收 chunk0 → resp_buf.extend(chunk0)
   rx 收 chunk1 → resp_buf.extend(chunk1)
   rx 收 chunk2 → resp_buf.extend(chunk2)
   ...直到 tx drop，rx 返回 None，循环结束
        │
   resp_buf = chunk0 + chunk1 + chunk2 + ...（完整响应 body）
        │
        ▼
   body_to_json(resp_buf) → Value
        │
        ▼
   组装 JSONL 行:
   {
     "id": 1,
     "method": "POST",
     "path": "/v1/chat/completions",
     "request_body":  { ... },
     "response_body": { ... },
     "duration_ms": 3421,
     "streaming": true
   }
        │
        ▼
   trace_lock.lock()  // 互斥写文件
   writeln!(file, json_line)
        │
        ▼
   eval_tx.send(TurnRecord { id, request_body, response_body, duration_ms })
        │
        │  ──── mpsc::unbounded_channel ────
        ▼
   eval::run() 收到 TurnRecord
        │
        ▼
   SessionBuilder.process() → 增量写 .view.json
        │
        ▼
   检测到 session 边界（消息回退 / 2分钟超时）
        │
        ▼
   seal_and_grade_bg() → 最终 .view.json 落盘
        │
        ▼
   tokio::spawn(grader::run_pipeline())
        │
        ▼
   Step 1: rules::extract_metrics()   → MetricsSnapshot
   Step 2: judge::judge()             → LLM 评审（task_completion + response_quality）
   Step 3: 加权汇总                    → GradeReport
        │
        ▼
   write_grade_json() → .grade.json 落盘
```

## 关键设计点

| 设计 | 说明 |
|---|---|
| **流式 tee** | `tx.send(chunk)` 在 `.map()` 里执行，不影响 Frame 管道。chunk 一到就推客户端，不缓冲 |
| **无界 channel** | `mpsc::unbounded_channel()` 确保 `send` 永不阻塞，不会因为后台慢而卡客户端响应 |
| **后台 task** | `tokio::spawn` 异步拼 body + 写 JSONL，主线程不等待 |
| **trace_lock** | 多请求并发时 JSONL 写入互斥，防止行交错 |
| **eval_tx 解耦** | proxy 只管发 TurnRecord，不关心 eval 处理快慢 |
| **后台评分** | `seal_and_grade_bg()` 用 `tokio::spawn` 跑 grader，不阻塞 session 检测主循环 |
| **no_proxy** | proxy 的 reqwest Client 和 grader 的独立 Client 都设 `no_proxy()`，避免自身流量被代理捕获 |
