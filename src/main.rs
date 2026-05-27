mod config;
mod eval;
mod format;
mod grader;
mod proxy;

use std::path::Path;
use std::sync::Arc;

use axum::{routing::any, Router};

use config::{Config, GraderConfig};
use eval::types::TurnRecord;
use proxy::AppState;

#[tokio::main]
async fn main() {
    let config = Config::load();

    std::fs::create_dir_all(&config.log_dir).expect("Failed to create log directory");

    let (eval_tx, eval_rx) = tokio::sync::mpsc::unbounded_channel::<TurnRecord>();
    let log_dir = config.log_dir.clone();

    let state = Arc::new(AppState::new(&config, eval_tx));

    // 从 trace_file 中提取 jsonl stem，用于 session 文件命名
    let jsonl_stem = Path::new(&state.trace_file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session_unknown")
        .to_string();

    let grader_config = GraderConfig::load(&config.upstream);

    // Spawn eval consumer: 异步构建结构化会话视图 + 评分
    tokio::spawn(eval::run(eval_rx, log_dir, jsonl_stem, grader_config));

    let app = Router::new()
        .fallback(any(proxy::handler))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();

    println!("listening http://{}  ->  {}", addr, config.upstream);

    axum::serve(listener, app).await.unwrap();
}
