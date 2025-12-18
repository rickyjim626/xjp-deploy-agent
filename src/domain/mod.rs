//! 领域模型模块
//!
//! 纯数据结构，不依赖 axum/tokio

pub mod deploy;
pub mod system;
pub mod container;
pub mod database;
pub mod tunnel;

// Re-exports for convenience
pub use deploy::{DeployStatus, DeployStage, DeployTask, DeployType, LogLine, StageStatus};
pub use system::{DiskInfo, LoadAverage, SystemInfo, SystemStats};
pub use container::{ContainerInfo, ContainerLogsQuery, EnvVar};
