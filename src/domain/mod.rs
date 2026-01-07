//! 领域模型模块
//!
//! 纯数据结构，不依赖 axum/tokio

pub mod code;
pub mod container;
pub mod database;
pub mod deploy;
pub mod system;
pub mod tunnel;

// Re-exports for convenience
pub use code::{CodeRepo, CodeStats, FileContent, FileNode, GitBranch, GitCommit, SearchResult};
pub use container::{ContainerInfo, ContainerLogsQuery, EnvVar};
pub use deploy::{DeployStatus, DeployStage, DeployTask, DeployType, LogLine, StageStatus};
pub use system::{DiskInfo, LoadAverage, SystemInfo, SystemStats};
