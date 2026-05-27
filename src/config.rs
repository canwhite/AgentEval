use std::env;

const DEFAULT_PORT: u16 = 57633;

pub struct Config {
    pub upstream: String,
    pub port: u16,
    pub log_dir: String,
    pub verbose: bool,
}

impl Config {
    pub fn load() -> Self {
        dotenvy::dotenv().ok();

        let upstream = env::var("AGENTEVAL_UPSTREAM")
            .unwrap_or_else(|_| "https://api.deepseek.com".into())
            .trim_end_matches('/')
            .to_string();

        let port = env::var("AGENTEVAL_PORT")
            .ok()
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
