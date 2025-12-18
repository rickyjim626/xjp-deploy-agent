//! 配置模块
//!
//! 环境变量解析与配置管理

pub mod env;
pub mod project;
pub mod autoupdate;

pub use env::EnvConfig;
pub use project::{ProjectConfig, RemoteAgentConfig};
pub use autoupdate::{AutoUpdateConfig, AutoUpdateState, AutoUpdateStatus, UpdateMetadata};
