use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::Response,
    routing::any,
    Router,
};
use clap::Parser;
use http_body::Frame;
use http_body_util::StreamBody;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;

const DEFAULT_PORT: u16 = 57633;

#[derive(Parser)]
#[command(
    name = "agenteval",
    about = "Transparent API proxy — no TLS hijack, just URL redirect"
)]
struct Cli {
    /// Upstream API base URL (e.g., https://api.anthropic.com)
    #[arg(short, long, default_value = "https://api.anthropic.com")]
    upstream: String,

    /// Port to listen on
    #[arg(short, long, default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Directory for request logs
    #[arg(long)]
    log_dir: Option<String>,

    /// Verbose output (print request/response bodies)
    #[arg(short, long)]
    verbose: bool,
}

struct AppState {
    upstream_base: String,
    client: Client,
    log_dir: String,
    verbose: bool,
    counter: AtomicU64,
}

fn default_log_dir() -> String {
    env::var("HOME")
        .map(|h| format!("{}/.agenteval/logs", h))
        .unwrap_or_else(|_| "/tmp/agenteval/logs".into())
}

fn timestamp_ms() -> u64 {
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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let log_dir = cli.log_dir.unwrap_or_else(default_log_dir);
    std::fs::create_dir_all(&log_dir).expect("Failed to create log directory");

    let client = Client::builder()
        .no_proxy()
        .build()
        .expect("Failed to create HTTP client");

    let upstream_base = cli.upstream.trim_end_matches('/').to_string();
    let state = Arc::new(AppState {
        upstream_base,
        client,
        log_dir,
        verbose: cli.verbose,
        counter: AtomicU64::new(1),
    });

    let app = Router::new()
        .fallback(any(proxy_handler))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", cli.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();

    println!("listening http://{}  ->  {}", addr, cli.upstream);

    axum::serve(listener, app).await.unwrap();
}

async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, String)> {
    let start = Instant::now();
    let id = state.counter.fetch_add(1, Ordering::SeqCst);

    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let upstream_url = format!("{}{}", state.upstream_base, path_and_query);

    let streaming = is_streaming_request(&body);
    let req_body_json = body_to_json(&body);

    // Build log entry
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
    log.insert(
        "response_headers".into(),
        headers_to_value(&resp_headers),
    );
    log.insert(
        "duration_ms".into(),
        Value::Number((elapsed.as_millis() as u64).into()),
    );

    // Print to console
    if state.verbose {
        println!(
            "[{:04}] {} {} -> {}  status={}  streaming={}  {}ms",
            id,
            method,
            path_and_query,
            state.upstream_base,
            status.as_u16(),
            streaming,
            elapsed.as_millis()
        );
        if !body.is_empty() {
            println!(
                "  req: {}",
                serde_json::to_string_pretty(&req_body_json).unwrap_or_default()
            );
        }
    } else {
        println!(
            "[{:04}] {} {}  {}  {}ms",
            id,
            method,
            path_and_query,
            status.as_u16(),
            elapsed.as_millis()
        );
    }

    // Stream response back (wrap bytes in http_body Frames)
    let frame_stream = upstream_resp.bytes_stream().map(|result| {
        match result {
            Ok(bytes) => Ok(Frame::data(bytes)),
            Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
        }
    });

    let axum_body = Body::new(StreamBody::new(frame_stream));

    let mut response = Response::new(axum_body);
    *response.status_mut() = status;
    *response.headers_mut() = resp_headers;

    write_log(&state.log_dir, id, &log);

    Ok(response)
}

fn write_log(log_dir: &str, id: u64, log: &serde_json::Map<String, Value>) {
    let path = format!("{}/{:04}.json", log_dir, id);
    if let Ok(json) = serde_json::to_string_pretty(log) {
        std::fs::write(&path, json).ok();
    }
}
