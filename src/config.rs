// 配置模块，负责加载环境变量并提供配置结构体
use std::env;

const DEFAULT_PORT: u16 = 57633;

pub struct Config {
    pub upstream: String,
    pub port: u16,
    pub log_dir: String,
    pub verbose: bool,
}

impl Config {
    // 加载配置，优先从环境变量获取，如果没有则使用默认值
    pub fn load() -> Self {
        // 加载环境变量，支持从 .env 文件加载
        dotenvy::dotenv().ok();

        let upstream = env::var("AGENTEVAL_UPSTREAM")
            .unwrap_or_else(|_| "https://api.deepseek.com".into())
            .trim_end_matches('/') // 去掉末尾的斜杠，保持一致性
            .to_string();

        let port = env::var("AGENTEVAL_PORT")
            .ok()//
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_PORT);

        let log_dir = env::var("AGENTEVAL_LOG_DIR").unwrap_or_else(|_| {
            env::var("HOME")
                .map(|h| format!("{}/.agenteval/logs", h))
                .unwrap_or_else(|_| "/tmp/agenteval/logs".into())
        });

        let verbose = env::var("AGENTEVAL_VERBOSE")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);

        Self { upstream, port, log_dir, verbose }
    }
}

/// grader 评测 LLM 配置
#[derive(Clone)]
pub struct GraderConfig {
    pub judge_api_base: String,
    pub judge_model: String,
    pub judge_api_key: String,
}

impl GraderConfig {
    pub fn load(upstream: &str) -> Self {
        dotenvy::dotenv().ok();

        let judge_api_base = env::var("AGENTEVAL_JUDGE_API_BASE")
            .unwrap_or_else(|_| upstream.to_string())
            .trim_end_matches('/')
            .to_string();

        let judge_model = env::var("AGENTEVAL_JUDGE_MODEL")
            .unwrap_or_else(|_| "MiniMax-M2.5".to_string());

        let judge_api_key = env::var("AGENTEVAL_JUDGE_API_KEY")
            .unwrap_or_default();

        Self { judge_api_base, judge_model, judge_api_key }
    }
}
