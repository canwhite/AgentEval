mod config;
mod diagnose;
mod eval;
mod format;
mod grader;
mod probe;
mod proxy;
mod web;

use std::path::Path;
use std::sync::Arc;

use axum::{routing::{any, get, post}, Router};

use config::{Config, GraderConfig, ProbeConfig};
use eval::types::TurnRecord;
use proxy::AppState;

#[tokio::main]
async fn main() {
    // CLI: diagnose / probe subcommands
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "diagnose" {
        run_diagnose_cli(&args).await;
        return;
    }
    if args.len() >= 2 && args[1] == "probe" {
        run_probe_cli(&args).await;
        return;
    }

    let config = Config::load();

    std::fs::create_dir_all(&config.log_dir).expect("Failed to create log directory");

    let (eval_tx, eval_rx) = tokio::sync::mpsc::unbounded_channel::<TurnRecord>();
    let log_dir = config.log_dir.clone();

    let grader_config = GraderConfig::load(&config.upstream);
    let probe_config = ProbeConfig::load();

    let state = Arc::new(AppState::new(&config, eval_tx, grader_config.clone(), probe_config));

    // 从 trace_file 中提取 jsonl stem，用于 session 文件命名
    let jsonl_stem = Path::new(&state.trace_file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session_unknown")
        .to_string();

    // Spawn eval consumer: 异步构建结构化会话视图 + 评分
    tokio::spawn(eval::run(eval_rx, log_dir, jsonl_stem, grader_config));

    let app = if config.ui_enabled {
        Router::new()
            .route("/dashboard/", get(web::serve_ui))
            .route("/dashboard/api/sessions", get(web::list_sessions))
            .route("/dashboard/api/sessions/{session_id}", get(web::get_session))
            .route("/dashboard/api/sessions/{session_id}/grade", post(web::grade_session))
            .route("/dashboard/api/sessions/{session_id}/diagnose", post(web::diagnose_session).get(web::get_diagnose))
            .route("/dashboard/api/sessions/{session_id}/probe", post(web::probe_session).get(web::get_probe))
            .route("/dashboard/api/raw/{jsonl_stem}", get(web::get_raw_jsonl))
            .fallback(any(proxy::handler))
            .with_state(state)
    } else {
        Router::new()
            .fallback(any(proxy::handler))
            .with_state(state)
    };

    let addr = format!("127.0.0.1:{}", config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();

    println!("listening http://{}  ->  {}", addr, config.upstream);
    if config.ui_enabled {
        println!("dashboard http://{}/dashboard/", addr);
    }

    axum::serve(listener, app).await.unwrap();
}

async fn run_diagnose_cli(args: &[String]) {
    let session_id = match args.get(2) {
        Some(id) => id.as_str(),
        None => {
            eprintln!("Usage: cargo run -- diagnose <session_id> [--format terminal|json]");
            std::process::exit(1);
        }
    };

    let format = if args.len() >= 4 && args[3] == "--format" {
        args.get(4).map(|s| s.as_str()).unwrap_or("json")
    } else {
        "json"
    };

    let log_dir = std::env::var("AGENTEVAL_LOG_DIR").unwrap_or_else(|_| "./logs".to_string());
    let grader_config = config::GraderConfig::load("https://api.deepseek.com");

    match diagnose::run(session_id, &log_dir, &grader_config).await {
        Ok(report) => match format {
            "terminal" => {
                println!("=== Diagnose Report: {} ===", report.session_id);
                println!("Diagnosed at: {}", report.diagnosed_at);
                println!(
                    "Issues: {} total ({} errors, {} warnings, {} infos)",
                    report.summary.total_issues,
                    report.summary.errors,
                    report.summary.warnings,
                    report.summary.infos
                );
                if report.issues.is_empty() {
                    println!("✓ No issues found.");
                } else {
                    for (_i, issue) in report.issues.iter().enumerate() {
                        let icon = match issue.severity {
                            diagnose::types::Severity::Error => "🔴",
                            diagnose::types::Severity::Warn => "🟡",
                            diagnose::types::Severity::Info => "🔵",
                        };
                        println!();
                        println!(
                            "{} [{:?}] {}",
                            icon, issue.category, issue.title
                        );
                        println!("   {}", issue.detail);
                        if !issue.evidence.is_empty() {
                            println!("   Evidence: {}", issue.evidence);
                        }
                        println!(
                            "   Location: jsonl_id={:?}, turn={:?}, step={:?}",
                            issue.location.jsonl_id,
                            issue.location.turn_id,
                            issue.location.step_index
                        );
                    }
                    if let Some(ref summary) = report.llm_summary {
                        println!();
                        println!("--- LLM Summary ---");
                        println!("{}", summary);
                    }
                }
            }
            _ => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
            }
        },
        Err(e) => {
            eprintln!("Diagnose failed: {}", e);
            std::process::exit(1);
        }
    }
}

async fn run_probe_cli(args: &[String]) {
    let session_id = match args.get(2) {
        Some(id) => id.as_str(),
        None => {
            eprintln!("Usage: cargo run -- probe <session_id>");
            std::process::exit(1);
        }
    };

    let log_dir = std::env::var("AGENTEVAL_LOG_DIR").unwrap_or_else(|_| "./logs".to_string());
    let probe_config = config::ProbeConfig::load();
    let grader_config = config::GraderConfig::load("https://api.deepseek.com");

    eprintln!("Running probe for session: {}", session_id);
    eprintln!("Source project: {}", probe_config.source_project_dir);

    match probe::run(session_id, &log_dir, &probe_config, &grader_config).await {
        Ok(report) => {
            println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            if report.parse_error.is_some() {
                eprintln!("Warning: probe output parse error, raw content preserved");
            }
        }
        Err(e) => {
            eprintln!("Probe failed: {}", e);
            std::process::exit(1);
        }
    }
}
