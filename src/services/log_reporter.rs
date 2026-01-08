//! 日志上报服务
//!
//! 定期收集 journalctl 日志并上报到部署中心，用于离线时查看历史日志。
//!
//! 工作流程：
//! 1. 每 30 秒执行一次
//! 2. 使用 journalctl --since 获取最近 30 秒的新日志
//! 3. 批量上报到 Deploy Center
//! 4. 记录上次上报时间，避免重复

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::process::Command;
use tokio::sync::RwLock;

use crate::state::AppState;

/// 上报间隔（秒）
const REPORT_INTERVAL_SECS: u64 = 30;

/// 每次上报最大行数
const MAX_LINES_PER_BATCH: usize = 200;

/// 日志上报状态
pub struct LogReporterState {
    /// 上次上报时间
    pub last_report: RwLock<Option<DateTime<Utc>>>,
    /// 上次上报结果
    pub last_result: RwLock<Option<String>>,
}

impl Default for LogReporterState {
    fn default() -> Self {
        Self {
            last_report: RwLock::new(None),
            last_result: RwLock::new(None),
        }
    }
}

/// 上报到 Deploy Center 的日志格式
#[derive(Debug, Serialize)]
struct ReportLogsRequest {
    agent_id: String,
    agent_url: String,
    logs: Vec<LogLineEntry>,
}

#[derive(Debug, Serialize)]
struct LogLineEntry {
    line: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<DateTime<Utc>>,
}

/// 启动日志上报任务
pub async fn start(state: Arc<AppState>) {
    // 检查是否配置了 callback_url
    let callback_url = match state.deploy_center.callback_url() {
        Some(url) => url.to_string(),
        None => {
            tracing::info!("Log reporter disabled: no callback URL configured");
            return;
        }
    };

    // 获取 agent 标识信息
    let agent_id = get_agent_id();
    let agent_url = get_agent_url(&state);

    tracing::info!(
        callback_url = %callback_url,
        agent_id = %agent_id,
        agent_url = %agent_url,
        interval_secs = REPORT_INTERVAL_SECS,
        "Starting log reporter service"
    );

    let reporter_state = Arc::new(LogReporterState::default());
    let mut interval = tokio::time::interval(Duration::from_secs(REPORT_INTERVAL_SECS));

    // 首次等待一个周期后再开始上报
    interval.tick().await;

    loop {
        interval.tick().await;

        if let Err(e) = report_logs(
            &state,
            &callback_url,
            &agent_id,
            &agent_url,
            &reporter_state,
        )
        .await
        {
            tracing::warn!(error = %e, "Failed to report logs");
            *reporter_state.last_result.write().await = Some(format!("Error: {}", e));
        }
    }
}

/// 执行一次日志上报
async fn report_logs(
    state: &AppState,
    callback_url: &str,
    agent_id: &str,
    agent_url: &str,
    reporter_state: &LogReporterState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let now = Utc::now();
    let since = {
        let last = reporter_state.last_report.read().await;
        match *last {
            Some(t) => t,
            None => now - chrono::Duration::seconds(REPORT_INTERVAL_SECS as i64),
        }
    };

    // 获取日志
    let logs = fetch_journalctl_logs(&since).await?;

    if logs.is_empty() {
        tracing::debug!("No new logs to report");
        *reporter_state.last_report.write().await = Some(now);
        *reporter_state.last_result.write().await = Some("No new logs".to_string());
        return Ok(());
    }

    // 构建请求
    let request = ReportLogsRequest {
        agent_id: agent_id.to_string(),
        agent_url: agent_url.to_string(),
        logs: logs
            .into_iter()
            .take(MAX_LINES_PER_BATCH)
            .map(|line| {
                // 尝试从日志行解析时间戳
                let timestamp = parse_journalctl_timestamp(&line);
                LogLineEntry { line, timestamp }
            })
            .collect(),
    };

    let log_count = request.logs.len();

    // 发送到 Deploy Center
    let url = format!(
        "{}/api/deploy/agents/{}/service-logs",
        callback_url, agent_id
    );

    let client = reqwest::Client::new();
    let response = client
        .post(&url)
        .json(&request)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;

    if response.status().is_success() {
        tracing::debug!(
            count = log_count,
            "Successfully reported agent logs"
        );
        *reporter_state.last_report.write().await = Some(now);
        *reporter_state.last_result.write().await = Some(format!("Reported {} logs", log_count));
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {}", status, body).into());
    }

    Ok(())
}

/// 使用 journalctl 获取指定时间之后的日志
async fn fetch_journalctl_logs(
    since: &DateTime<Utc>,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    // 格式化时间为 journalctl 接受的格式
    let since_str = since.format("%Y-%m-%d %H:%M:%S").to_string();

    let mut cmd = Command::new("journalctl");
    cmd.args([
        "-u",
        "xjp-deploy-agent",
        "--since",
        &since_str,
        "--no-pager",
        "-o",
        "short-iso",
        "-n",
        &MAX_LINES_PER_BATCH.to_string(),
    ]);

    let result = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await;

    match result {
        Ok(Ok(output)) => {
            if !output.status.success() {
                // journalctl 可能失败（权限问题等），返回空日志
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::debug!(stderr = %stderr, "journalctl returned non-zero");
                return Ok(Vec::new());
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<String> = stdout
                .lines()
                .filter(|l| !l.is_empty())
                .map(|s| s.to_string())
                .collect();

            Ok(lines)
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "Failed to execute journalctl");
            Ok(Vec::new())
        }
        Err(_) => {
            tracing::debug!("journalctl command timed out");
            Ok(Vec::new())
        }
    }
}

/// 尝试从 journalctl 输出解析时间戳
///
/// journalctl -o short-iso 输出格式: 2024-01-08T22:42:15+0800 hostname service[pid]: message
fn parse_journalctl_timestamp(line: &str) -> Option<DateTime<Utc>> {
    // 尝试提取开头的时间戳
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    if parts.is_empty() {
        return None;
    }

    // 尝试解析 ISO 格式时间戳
    chrono::DateTime::parse_from_str(parts[0], "%Y-%m-%dT%H:%M:%S%z")
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// 获取 Agent ID（使用 hostname）
fn get_agent_id() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// 获取 Agent URL
fn get_agent_url(state: &AppState) -> String {
    // 尝试从配置构建 URL
    let port = state.config.port;
    let hostname = get_agent_id();

    // 如果是通过隧道访问，使用隧道 URL
    if state.tunnel_mode == crate::domain::tunnel::TunnelMode::Client {
        if let Some(callback_url) = state.deploy_center.callback_url() {
            // 构建隧道代理 URL
            return format!(
                "{}/tunnel/proxy/{}",
                callback_url, state.tunnel_client_id
            );
        }
    }

    format!("http://{}:{}", hostname, port)
}
