//! orbital-core：记忆系统的纯库（无网络、无 UI）。
//!
//! M1 阶段只落地"可编译 + 错误码统一 + 建库"三件事；
//! schema / writer / 召回等按 `docs/04` 在后续任务卡里实现。
#![forbid(unsafe_code)]

pub mod adapter;
pub mod attach;
pub mod config;
pub mod db;
pub mod embed;
pub mod error;
pub mod identity;
pub mod local_api;
pub mod schema;
pub mod service;
pub mod writer;

/// 库版本，`orbital --version` 与体检都会用到。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
