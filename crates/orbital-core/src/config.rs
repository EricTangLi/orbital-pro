//! 配置：所有可调参数集中在这里，**代码里不许出现魔法数字**（docs/04 §3）。

use std::env;

/// 默认端口（可配置；冲突时由启动器顺延并写回配置，见 docs/02 §3.2）。
pub const DEFAULT_CORE_PORT: u16 = 8791;
pub const DEFAULT_PROXY_PORT: u16 = 8790;

/// 单条记忆内容上限：**UTF-8 字节**（审核 P1-7，不是字符数）。
pub const MAX_CONTENT_BYTES: usize = 64 * 1024;

/// 写队列批提交阈值（ADR-003）。
pub const WRITER_BATCH_SIZE: usize = 100;
pub const WRITER_FLUSH_MS: u64 = 50;

/// 注入超时：超时直接跳过注入，绝不阻塞对话（ADR-008）。
pub const INJECT_TIMEOUT_MS: u64 = 300;

/// db 文件名（放在数据目录下）。
pub const DB_FILE_NAME: &str = "orbital.db";

#[derive(Debug, Clone)]
pub struct Config {
    pub core_port: u16,
    pub proxy_port: u16,
    pub data_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            core_port: env_u16("ORBITAL_CORE_PORT", DEFAULT_CORE_PORT),
            proxy_port: env_u16("ORBITAL_PROXY_PORT", DEFAULT_PROXY_PORT),
            data_dir: env::var("ORBITAL_DATA_DIR").unwrap_or_else(|_| default_data_dir()),
        }
    }
}

fn env_u16(key: &str, fallback: u16) -> u16 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(fallback)
}

fn default_data_dir() -> String {
    // %LOCALAPPDATA%\OrbitalPro\data（docs/02 §8）
    match env::var("LOCALAPPDATA") {
        Ok(base) => format!("{}\\OrbitalPro\\data", base),
        Err(_) => ".\\data".to_string(),
    }
}
