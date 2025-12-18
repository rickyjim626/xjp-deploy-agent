//! 基础设施模块
//!
//! 封装外部依赖（HTTP client、命令执行等）

pub mod deploy_center;
pub mod command;

pub use deploy_center::DeployCenterClient;
pub use command::CommandRunner;
