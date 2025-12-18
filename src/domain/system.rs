//! 系统信息领域模型

use chrono::{DateTime, Utc};
use serde::Serialize;

/// 系统配置信息（静态）
#[derive(Clone, Debug, Serialize)]
pub struct SystemInfo {
    pub hostname: String,
    pub os_name: String,
    pub os_version: String,
    pub kernel_version: String,
    pub cpu_arch: String,
    pub cpu_count: usize,
    pub cpu_brand: String,
    pub total_memory_gb: f64,
    pub total_swap_gb: f64,
}

/// 磁盘信息
#[derive(Clone, Debug, Serialize)]
pub struct DiskInfo {
    pub name: String,
    pub mount_point: String,
    pub total_gb: f64,
    pub used_gb: f64,
    pub available_gb: f64,
    pub usage_percent: f64,
}

/// 系统负载统计（动态）
#[derive(Clone, Debug, Serialize)]
pub struct SystemStats {
    pub timestamp: DateTime<Utc>,
    pub cpu_usage_percent: f64,
    pub memory_used_gb: f64,
    pub memory_total_gb: f64,
    pub memory_usage_percent: f64,
    pub swap_used_gb: f64,
    pub swap_total_gb: f64,
    pub swap_usage_percent: f64,
    pub disks: Vec<DiskInfo>,
    pub load_average: LoadAverage,
}

/// 系统负载平均值 (1, 5, 15 分钟)
#[derive(Clone, Debug, Serialize)]
pub struct LoadAverage {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

impl LoadAverage {
    pub fn new(one: f64, five: f64, fifteen: f64) -> Self {
        Self { one, five, fifteen }
    }
}
