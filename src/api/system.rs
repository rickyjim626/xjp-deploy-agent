//! 系统信息 API
//!
//! 包含 /system/info, /system/stats 端点

use axum::{
    extract::State,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use std::sync::Arc;
use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System};

use crate::config::env::constants::VERSION;
use crate::domain::system::{DiskInfo, LoadAverage, SystemInfo, SystemStats};
use crate::state::AppState;

/// 创建系统信息路由
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/system/info", get(get_system_info))
        .route("/system/stats", get(get_system_stats))
}

/// 获取系统硬件信息
///
/// GET /system/info
/// 无需认证
async fn get_system_info(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything()),
    );
    sys.refresh_cpu_all();

    let cpu_brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    let info = SystemInfo {
        hostname: System::host_name().unwrap_or_else(|| "unknown".to_string()),
        os_name: System::name().unwrap_or_else(|| "unknown".to_string()),
        os_version: System::os_version().unwrap_or_else(|| "unknown".to_string()),
        kernel_version: System::kernel_version().unwrap_or_else(|| "unknown".to_string()),
        cpu_arch: std::env::consts::ARCH.to_string(),
        cpu_count: sys.cpus().len(),
        cpu_brand,
        total_memory_gb: sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0,
        total_swap_gb: sys.total_swap() as f64 / 1024.0 / 1024.0 / 1024.0,
    };

    // 计算运行时间
    let uptime_secs = (chrono::Utc::now() - state.started_at).num_seconds();
    let uptime_str = format!(
        "{}d {}h {}m {}s",
        uptime_secs / 86400,
        (uptime_secs % 86400) / 3600,
        (uptime_secs % 3600) / 60,
        uptime_secs % 60
    );

    Json(serde_json::json!({
        "system": info,
        "agent": {
            "version": VERSION,
            "started_at": state.started_at.to_rfc3339(),
            "uptime": uptime_str,
            "projects": state.projects.keys().collect::<Vec<_>>(),
        }
    }))
}

/// 获取系统负载统计
///
/// GET /system/stats
/// 无需认证
async fn get_system_stats() -> impl IntoResponse {
    // 创建 System 实例并刷新所有需要的数据
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything()),
    );

    // 需要等待一小段时间让 CPU 使用率计算准确
    sys.refresh_cpu_all();
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    sys.refresh_cpu_all();

    // 获取磁盘信息
    let disks = Disks::new_with_refreshed_list();
    let disk_info: Vec<DiskInfo> = disks
        .iter()
        .map(|disk| {
            let total = disk.total_space() as f64 / 1024.0 / 1024.0 / 1024.0;
            let available = disk.available_space() as f64 / 1024.0 / 1024.0 / 1024.0;
            let used = total - available;
            DiskInfo {
                name: disk.name().to_string_lossy().to_string(),
                mount_point: disk.mount_point().to_string_lossy().to_string(),
                total_gb: total,
                used_gb: used,
                available_gb: available,
                usage_percent: if total > 0.0 {
                    (used / total) * 100.0
                } else {
                    0.0
                },
            }
        })
        .collect();

    // 计算 CPU 平均使用率
    let cpu_usage: f64 = if sys.cpus().is_empty() {
        0.0
    } else {
        sys.cpus().iter().map(|c| c.cpu_usage() as f64).sum::<f64>() / sys.cpus().len() as f64
    };

    let memory_total = sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
    let memory_used = sys.used_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
    let swap_total = sys.total_swap() as f64 / 1024.0 / 1024.0 / 1024.0;
    let swap_used = sys.used_swap() as f64 / 1024.0 / 1024.0 / 1024.0;

    // 获取负载平均值 (仅 Unix)
    let load_avg = System::load_average();

    let stats = SystemStats {
        timestamp: chrono::Utc::now(),
        cpu_usage_percent: cpu_usage,
        memory_used_gb: memory_used,
        memory_total_gb: memory_total,
        memory_usage_percent: if memory_total > 0.0 {
            (memory_used / memory_total) * 100.0
        } else {
            0.0
        },
        swap_used_gb: swap_used,
        swap_total_gb: swap_total,
        swap_usage_percent: if swap_total > 0.0 {
            (swap_used / swap_total) * 100.0
        } else {
            0.0
        },
        disks: disk_info,
        load_average: LoadAverage::new(load_avg.one, load_avg.five, load_avg.fifteen),
    };

    Json(stats)
}
