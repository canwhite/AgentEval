mod config;
mod proxy;

use std::sync::Arc;

use axum::{routing::any, Router};

use config::Config;
use proxy::AppState;

#[tokio::main]
async fn main() {
    let config = Config::load();

    std::fs::create_dir_all(&config.log_dir).expect("Failed to create log directory");

    let state = Arc::new(AppState::new(&config));

    //创建app，所有请求都交给proxy::handler处理，并共享状态
    let app = Router::new()
        .fallback(any(proxy::handler))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();

    println!("listening http://{}  ->  {}", addr, config.upstream);

    axum::serve(listener, app).await.unwrap();
}
