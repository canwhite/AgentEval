use clap::Parser;

const DEFAULT_PORT: u16 = 57633;

fn default_log_dir() -> String {
    std::env::var("HOME")
        .map(|h| format!("{}/.agenteval/logs", h))
        .unwrap_or_else(|_| "/tmp/agenteval/logs".into())
}

/// 透明 API 代理 —— 不改 TLS，劫持 URL
#[derive(Parser, Debug)]
#[command(name = "agenteval")]
pub struct Config {
    /// 上游 API 地址
    #[arg(short, long, env = "AGENTEVAL_UPSTREAM", default_value = "https://api.anthropic.com")]
    pub upstream: String,

    /// 本地监听端口
    #[arg(short, long, env = "AGENTEVAL_PORT", default_value_t = DEFAULT_PORT)]
    pub port: u16,

    /// 日志存放目录
    #[arg(long, env = "AGENTEVAL_LOG_DIR", default_value_t = default_log_dir())]
    pub log_dir: String,

    /// 详细模式（打印 request body）
    #[arg(short, long, env = "AGENTEVAL_VERBOSE", default_value_t = false)]
    pub verbose: bool,
}

impl Config {
    pub fn load() -> Self {
        // .env 加载到进程环境，clap 通过 env = "..." 自动读取
        dotenvy::dotenv().ok();
        let mut config = Self::parse();
        // 去掉 upstream 尾部斜杠，避免双斜杠
        config.upstream = config.upstream.trim_end_matches('/').to_string();
        config
    }
}
