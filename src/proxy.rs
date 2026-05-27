use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::Response,
};
use http_body::Frame;
use http_body_util::StreamBody;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;

use crate::config::Config;

pub struct AppState {
    pub upstream_base: String,
    pub client: Client,
    pub trace_file: String,    // 一个 session 一个文件
    pub trace_lock: Mutex<()>, // 串行写入
    pub verbose: bool,
    pub counter: std::sync::atomic::AtomicU64,
}

impl AppState {
    pub fn new(config: &Config) -> Self {
        let client = Client::builder()
            .no_proxy()
            .build()
            .expect("Failed to create HTTP client");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let trace_file = format!("{}/session_{}.jsonl", config.log_dir, now);
        std::fs::create_dir_all(&config.log_dir).expect("Failed to create log directory");

        Self {
            upstream_base: config.upstream.clone(),
            client,
            trace_file,
            trace_lock: Mutex::new(()),
            verbose: config.verbose,
            counter: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

pub async fn handler(
    State(state): State<std::sync::Arc<AppState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, String)> {
    let start = Instant::now();
    let id = state.counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let upstream_url = format!("{}{}", state.upstream_base, path_and_query);

    let streaming = is_streaming_request(&body);
    let req_body_json = body_to_json(&body);

    // Forward to upstream
    let mut req_builder = state.client.request(method.clone(), &upstream_url);

    for (key, value) in headers.iter() {
        let k = key.as_str().to_lowercase();
        if k == "host" || k == "content-length" || k == "transfer-encoding" {
            continue;
        }
        req_builder = req_builder.header(key.as_str(), value);
    }

    if !body.is_empty() {
        req_builder = req_builder.body(body.to_vec());
    }

    let upstream_resp = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[{:04}] upstream error: {}", id, e);
            return Err((StatusCode::BAD_GATEWAY, e.to_string()));
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let resp_headers_val = headers_to_value(&resp_headers);
    let elapsed = start.elapsed();

    // Console output
    if state.verbose {
        println!(
            "[{:04}] {} {} -> {}  status={}  streaming={}  {}ms",
            id, method, path_and_query, state.upstream_base, status.as_u16(), streaming, elapsed.as_millis()
        );
        if !body.is_empty() {
            println!("  req: {}", serde_json::to_string_pretty(&req_body_json).unwrap_or_default());
        }
    } else {
        println!(
            "[{:04}] {} {}  {}  {}ms",
            id, method, path_and_query, status.as_u16(), elapsed.as_millis()
        );
    }

    // Stream response back, tee chunks to channel for full-body capture
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let frame_stream = upstream_resp.bytes_stream().map(move |result| match result {
        Ok(bytes) => {
            tx.send(bytes.to_vec()).ok();
            Ok(Frame::data(bytes))
        }
        Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
    });

    let axum_body = Body::new(StreamBody::new(frame_stream));

    let mut response = Response::new(axum_body);
    *response.status_mut() = status;
    *response.headers_mut() = resp_headers;

    // Background: accumulate full response body, then write complete trace line
    let st = state.clone();
    let method_str = method.to_string();
    let path_str = path_and_query.to_string();
    let req_headers = headers_to_value(&headers);
    let status_u16 = status.as_u16();
    let elapsed_ms = elapsed.as_millis() as u64;

    tokio::spawn(async move {
        let mut resp_buf = Vec::new();
        while let Some(chunk) = rx.recv().await {
            resp_buf.extend_from_slice(&chunk);
        }

        let mut entry = serde_json::Map::new();
        entry.insert("id".into(), Value::Number(id.into()));
        entry.insert("ts".into(), Value::Number(timestamp_ms().into()));
        entry.insert("method".into(), Value::String(method_str));
        entry.insert("path".into(), Value::String(path_str));
        entry.insert("upstream".into(), Value::String(upstream_url));
        entry.insert("request_headers".into(), req_headers);
        entry.insert("request_body".into(), req_body_json);
        entry.insert("response_status".into(), Value::Number(status_u16.into()));
        entry.insert("response_headers".into(), resp_headers_val);
        entry.insert("response_body".into(), body_to_json(&resp_buf));
        entry.insert("duration_ms".into(), Value::Number(elapsed_ms.into()));
        entry.insert("streaming".into(), Value::Bool(streaming));

        let _guard = st.trace_lock.lock().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&st.trace_file)
            .unwrap();
        let line = serde_json::to_string(&Value::Object(entry)).unwrap();
        writeln!(file, "{}", line).ok();
    });

    Ok(response)
}

// ── helpers ──

fn timestamp_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn headers_to_value(headers: &HeaderMap) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in headers.iter() {
        let k = key.as_str().to_string();
        let v = value.to_str().unwrap_or("<binary>").to_string();
        map.insert(k, Value::String(v));
    }
    Value::Object(map)
}

fn body_to_json(body: &[u8]) -> Value {
    if body.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(body).unwrap_or_else(|_| match std::str::from_utf8(body) {
        Ok(s) => Value::String(s.to_string()),
        Err(_) => Value::String(format!("<binary {} bytes>", body.len())),
    })
}

fn is_streaming_request(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false)
}
