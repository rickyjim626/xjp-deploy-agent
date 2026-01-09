//! 自动更新服务 (蓝绿部署策略)
//!
//! 定期检查更新并使用蓝绿部署策略自动应用：
//! 1. 下载新二进制到临时位置
//! 2. 启动 canary 实例进行健康检查
//! 3. 健康检查通过后替换并重启
//! 4. 失败则保持当前版本

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::autoupdate::{AutoUpdateConfig, UpdateMetadata};
use crate::config::env::constants::VERSION;
use crate::state::AppState;

/// Canary 健康检查配置
const CANARY_PORT: u16 = 19999;
const CANARY_STARTUP_WAIT_SECS: u64 = 5;
const CANARY_HEALTH_CHECK_TIMEOUT_SECS: u64 = 10;

/// 手动触发更新（供 API 调用）
pub async fn trigger_update(
    state: Arc<AppState>,
    config: AutoUpdateConfig,
    metadata: UpdateMetadata,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::info!(
        version = %metadata.version,
        "Manual update triggered via API"
    );
    download_and_apply_update(&state, &config, &metadata).await
}

/// 启动自动更新任务
pub async fn start(state: Arc<AppState>) {
    let config = match &state.auto_update_config {
        Some(config) => config.clone(),
        None => {
            tracing::info!("Auto-update disabled");
            return;
        }
    };

    tracing::info!(
        endpoint = %config.endpoint,
        check_interval = config.check_interval_secs,
        "Starting auto-update task (blue-green strategy)"
    );

    let mut interval =
        tokio::time::interval(std::time::Duration::from_secs(config.check_interval_secs));

    loop {
        interval.tick().await;

        // 更新下次检查时间
        {
            let next = chrono::Utc::now()
                + chrono::Duration::seconds(config.check_interval_secs as i64);
            *state.auto_update_state.next_check.write().await = Some(next);
        }

        // 检查更新
        if let Err(e) = check_and_apply_update(&state, &config).await {
            tracing::warn!(error = %e, "Auto-update check failed");
            *state.auto_update_state.last_check_result.write().await =
                Some(format!("Error: {}", e));
        }
    }
}

/// 检查并应用更新 (蓝绿部署)
async fn check_and_apply_update(
    state: &Arc<AppState>,
    config: &AutoUpdateConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 更新检查时间
    *state.auto_update_state.last_check.write().await = Some(chrono::Utc::now());

    // 获取元数据
    let metadata = fetch_update_metadata(&config.metadata_url()).await?;

    // 检查版本
    *state.auto_update_state.latest_version.write().await = Some(metadata.version.clone());

    if !is_newer_version(&metadata.version, VERSION) {
        *state.auto_update_state.update_available.write().await = false;
        *state.auto_update_state.last_check_result.write().await =
            Some("Already up to date".to_string());
        tracing::info!(
            current = VERSION,
            latest = %metadata.version,
            "No update available"
        );
        return Ok(());
    }

    *state.auto_update_state.update_available.write().await = true;
    tracing::info!(
        current = VERSION,
        latest = %metadata.version,
        "Update available, starting blue-green deployment"
    );

    // 执行蓝绿部署更新
    match download_and_apply_update(state, config, &metadata).await {
        Ok(()) => {
            *state.auto_update_state.last_check_result.write().await =
                Some(format!("Successfully updated to {}", metadata.version));
            tracing::info!(version = %metadata.version, "Update applied successfully");
        }
        Err(e) => {
            *state.auto_update_state.last_check_result.write().await =
                Some(format!("Update failed: {}", e));
            tracing::error!(error = %e, "Failed to apply update");
            return Err(e);
        }
    }

    Ok(())
}

/// 下载并应用更新 (蓝绿部署策略)
async fn download_and_apply_update(
    state: &Arc<AppState>,
    config: &AutoUpdateConfig,
    metadata: &UpdateMetadata,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let binary_url = config.binary_url(&metadata.version);

    // Step 1: 下载新二进制
    *state.auto_update_state.update_progress.write().await = "downloading".to_string();
    tracing::info!(url = %binary_url, "Downloading new binary");

    let temp_dir = std::env::temp_dir();
    let temp_binary_path = temp_dir.join(format!("xjp-deploy-agent-{}", metadata.version));

    download_binary(&binary_url, &temp_binary_path).await?;

    // Step 2: 验证校验和 (如果有)
    if let Some(ref expected_sha256) = metadata.sha256 {
        *state.auto_update_state.update_progress.write().await = "verifying".to_string();
        tracing::info!("Verifying checksum");

        verify_checksum(&temp_binary_path, expected_sha256).await?;
        tracing::info!("Checksum verified");
    }

    // Step 3: 蓝绿健康检查 - 启动 canary 实例
    *state.auto_update_state.update_progress.write().await = "health_check".to_string();
    tracing::info!("Starting canary health check");

    let canary_healthy = run_canary_health_check(&temp_binary_path).await;

    if !canary_healthy {
        // 清理临时文件
        let _ = fs::remove_file(&temp_binary_path).await;
        *state.auto_update_state.update_progress.write().await = "none".to_string();
        return Err("Canary health check failed - keeping current version".into());
    }

    tracing::info!("Canary health check passed");

    // Step 4: 替换当前二进制
    *state.auto_update_state.update_progress.write().await = "applying".to_string();
    tracing::info!("Applying update - replacing binary");

    let current_binary = std::env::current_exe()?;
    let backup_binary = current_binary.with_extension("bak");

    // 备份当前二进制
    if current_binary.exists() {
        fs::rename(&current_binary, &backup_binary).await?;
    }

    // 移动新二进制到当前位置
    if let Err(e) = fs::rename(&temp_binary_path, &current_binary).await {
        // 恢复备份
        if backup_binary.exists() {
            let _ = fs::rename(&backup_binary, &current_binary).await;
        }
        return Err(format!("Failed to replace binary: {}", e).into());
    }

    // 设置执行权限 (Unix only)
    #[cfg(unix)]
    fs::set_permissions(&current_binary, std::fs::Permissions::from_mode(0o755)).await?;

    // 清理备份
    let _ = fs::remove_file(&backup_binary).await;

    *state.auto_update_state.update_progress.write().await = "restarting".to_string();
    tracing::info!("Update applied, restarting...");

    // Step 5: 重启进程
    // 使用 exec 系统调用替换当前进程
    restart_self(&current_binary).await?;

    Ok(())
}

/// 下载二进制文件
async fn download_binary(
    url: &str,
    dest: &PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let response = reqwest::get(url).await?;

    if !response.status().is_success() {
        return Err(format!("Download failed with status: {}", response.status()).into());
    }

    let bytes = response.bytes().await?;

    let mut file = fs::File::create(dest).await?;
    file.write_all(&bytes).await?;

    // 设置执行权限 (Unix only)
    #[cfg(unix)]
    fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755)).await?;

    tracing::info!(
        path = %dest.display(),
        size = bytes.len(),
        "Binary downloaded"
    );

    Ok(())
}

/// 验证文件校验和
async fn verify_checksum(
    path: &PathBuf,
    expected: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let contents = fs::read(path).await?;
    let mut hasher = Sha256::new();
    hasher.update(&contents);
    let result = hasher.finalize();
    let actual = format!("{:x}", result);

    if actual != expected.to_lowercase() {
        return Err(format!(
            "Checksum mismatch: expected {}, got {}",
            expected, actual
        )
        .into());
    }

    Ok(())
}

/// 运行 canary 健康检查
///
/// 启动新二进制进行健康检查，验证它能正常启动并响应
async fn run_canary_health_check(binary_path: &PathBuf) -> bool {
    tracing::info!(
        path = %binary_path.display(),
        port = CANARY_PORT,
        "Starting canary instance for health check"
    );

    // 启动 canary 进程
    // 使用命令行参数 --port 和 --canary，避免环境变量被 .env 文件覆盖
    let canary_result = Command::new(binary_path)
        .args(["--port", &CANARY_PORT.to_string(), "--canary"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match canary_result {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "Failed to start canary process");
            return false;
        }
    };

    // 等待启动
    tracing::info!(
        wait_secs = CANARY_STARTUP_WAIT_SECS,
        "Waiting for canary to start"
    );
    tokio::time::sleep(std::time::Duration::from_secs(CANARY_STARTUP_WAIT_SECS)).await;

    // 检查进程是否还在运行
    match child.try_wait() {
        Ok(Some(status)) => {
            tracing::error!(
                exit_code = ?status.code(),
                "Canary process exited prematurely"
            );
            return false;
        }
        Ok(None) => {
            // 进程还在运行，继续检查
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to check canary process status");
            let _ = child.kill().await;
            return false;
        }
    }

    // 执行健康检查请求
    let health_url = format!("http://127.0.0.1:{}/health", CANARY_PORT);
    tracing::info!(url = %health_url, "Checking canary health endpoint");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(CANARY_HEALTH_CHECK_TIMEOUT_SECS))
        .build()
        .unwrap();

    let health_check_result = client.get(&health_url).send().await;

    // 无论结果如何，先终止 canary 进程
    let _ = child.kill().await;
    let _ = child.wait().await;

    match health_check_result {
        Ok(response) => {
            if response.status().is_success() {
                tracing::info!("Canary health check passed");
                true
            } else {
                tracing::error!(
                    status = %response.status(),
                    "Canary health check failed with non-success status"
                );
                false
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "Canary health check request failed");
            false
        }
    }
}

/// 重启当前进程
///
/// 使用统一的 RestartManager 处理不同平台的重启逻辑
async fn restart_self(binary_path: &PathBuf) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crate::services::restart::{RestartManager, RestartMode};

    tracing::info!(
        binary = %binary_path.display(),
        "Restarting process via RestartManager"
    );

    // 检测运行模式
    #[cfg(windows)]
    let mode = {
        use crate::services::windows_service;
        if windows_service::is_running_as_service() {
            tracing::info!("Detected Windows service mode");
            RestartMode::Service
        } else {
            tracing::info!("Detected Windows console mode");
            RestartMode::Console(binary_path.clone())
        }
    };

    #[cfg(unix)]
    let mode = {
        // Unix 系统检测是否作为 systemd 服务运行
        if std::env::var("INVOCATION_ID").is_ok() {
            tracing::info!("Detected systemd service mode");
            RestartMode::Service
        } else {
            tracing::info!("Detected console mode");
            RestartMode::Console(binary_path.clone())
        }
    };

    #[cfg(not(any(unix, windows)))]
    let mode = RestartMode::Console(binary_path.clone());

    // 调度重启
    RestartManager::schedule_restart(mode)
        .await
        .map_err(|e| format!("Failed to schedule restart: {}", e))?;

    // 给重启调度一点时间启动
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    tracing::info!("Exiting for restart...");
    std::process::exit(0);
}

// Note: Windows restart functions have been moved to services::restart::RestartManager

/// 获取更新元数据
async fn fetch_update_metadata(
    url: &str,
) -> Result<UpdateMetadata, Box<dyn std::error::Error + Send + Sync>> {
    let response = reqwest::get(url).await?;
    let metadata: UpdateMetadata = response.json().await?;
    Ok(metadata)
}

/// 比较版本号
fn is_newer_version(latest: &str, current: &str) -> bool {
    let parse_version = |v: &str| -> Vec<u32> {
        v.split('.')
            .filter_map(|s| s.parse().ok())
            .collect()
    };

    let latest_parts = parse_version(latest);
    let current_parts = parse_version(current);

    for (l, c) in latest_parts.iter().zip(current_parts.iter()) {
        match l.cmp(c) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => continue,
        }
    }

    latest_parts.len() > current_parts.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("1.0.1", "1.0.0"));
        assert!(is_newer_version("1.1.0", "1.0.0"));
        assert!(is_newer_version("2.0.0", "1.9.9"));
        assert!(!is_newer_version("1.0.0", "1.0.0"));
        assert!(!is_newer_version("0.9.9", "1.0.0"));
    }
}
