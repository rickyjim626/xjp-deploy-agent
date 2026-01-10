//! 服务层模块
//!
//! 包含核心业务逻辑

pub mod deploy;
pub mod autoupdate;
pub mod frp;
pub mod mesh;
pub mod nfa;
pub mod ssh;
pub mod tunnel;
pub mod log_reporter;
pub mod restart;
pub mod lan_discovery;
pub mod service_router;
pub mod queue_persistence;

#[cfg(windows)]
pub mod windows_service;
