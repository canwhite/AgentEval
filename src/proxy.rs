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
    pub log_dir: String,
    pub verbose: bool,
    pub counter: std::sync::atomic::AtomicU64,
}

impl AppState {
    pub fn new(config: &Config) -> Self {
        let client = Client::builder()
            .no_proxy()
            .build()
            .expect("Failed to create HTTP client");

        Self {
            upstream_base: config.upstream.clone(),
            client,
            log_dir: config.log_dir.clone(),
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

    let mut log = serde_json::Map::new();
    log.insert("id".into(), Value::Number(id.into()));
    log.insert("ts".into(), Value::Number(timestamp_ms().into()));
    log.insert("method".into(), Value::String(method.to_string()));
    log.insert("path".into(), Value::String(path_and_query.to_string()));
    log.insert("upstream".into(), Value::String(upstream_url.clone()));
    log.insert("request_headers".into(), headers_to_value(&headers));
    log.insert("request_body".into(), req_body_json.clone());
    log.insert("streaming".into(), Value::Bool(streaming));

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
            let elapsed = start.elapsed();
            log.insert("error".into(), Value::String(e.to_string()));
            log.insert("duration_ms".into(), Value::Number((elapsed.as_millis() as u64).into()));
            write_log(&state.log_dir, id, &log);
            eprintln!("[{:04}] upstream error: {}", id, e);
            return Err((StatusCode::BAD_GATEWAY, e.to_string()));
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let elapsed = start.elapsed();

    log.insert("response_status".into(), Value::Number(status.as_u16().into()));
    log.insert("response_headers".into(), headers_to_value(&resp_headers));
    log.insert("duration_ms".into(), Value::Number((elapsed.as_millis() as u64).into()));

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

    // Stream response back, tee each chunk to a channel for logging
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

    // Write initial log (no response_body yet)
    write_log(&state.log_dir, id, &log);

    // Background: accumulate response chunks, update log when stream ends
    let log_dir = state.log_dir.clone();
    tokio::spawn(async move {
        let mut buf = Vec::new();
        while let Some(chunk) = rx.recv().await {
            buf.extend_from_slice(&chunk);
        }
        update_log_response_body(&log_dir, id, &buf);
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

fn write_log(log_dir: &str, id: u64, log: &serde_json::Map<String, Value>) {
    let path = format!("{}/{:04}.json", log_dir, id);
    if let Ok(json) = serde_json::to_string_pretty(log) {
        std::fs::write(&path, json).ok();
    }
}

fn update_log_response_body(log_dir: &str, id: u64, body_bytes: &[u8]) {
    let path = format!("{}/{:04}.json", log_dir, id);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut value: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return,
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert("response_body".into(), body_to_json(body_bytes));
        if let Ok(json) = serde_json::to_string_pretty(&value) {
            std::fs::write(&path, json).ok();
        }
    }
}
