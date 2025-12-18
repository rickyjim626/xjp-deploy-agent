//! 运行时状态模块
//!
//! 管理应用状态、任务存储和日志通道

pub mod app_state;
pub mod log_hub;
pub mod task_store;
pub mod tunnel_state;

pub use app_state::AppState;
pub use log_hub::LogHub;
pub use task_store::TaskStore;
pub use tunnel_state::{TunnelClientState, TunnelServerState};
