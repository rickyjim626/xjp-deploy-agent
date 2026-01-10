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
        "Update available, checking if safe to apply"
    );

    // 检查是否有任务正在运行
    let running_deploys = state.running_deploys.read().await;
    if !running_deploys.is_empty() {
        let running_projects: Vec<_> = running_deploys.keys().cloned().collect();
        drop(running_deploys);

        // 延迟更新：设置 pending_update，等待任务完成后自动触发
        tracing::info!(
            running_projects = ?running_projects,
            version = %metadata.version,
            "Deferring update: tasks are running. Will apply after completion."
        );

        state.auto_update_state.set_pending_update(Some(metadata.clone())).await;
        *state.auto_update_state.update_progress.write().await = "deferred".to_string();
        *state.auto_update_state.last_check_result.write().await =
            Some(format!("Update to {} deferred - waiting for {} running task(s)",
                metadata.version, running_projects.len()));

        return Ok(());
    }
    drop(running_deploys);

    // 没有运行中的任务，执行更新
    apply_update_internal(state, config, &metadata).await
}

/// 内部更新执行函数（供正常更新和延迟更新使用）
async fn apply_update_internal(
    state: &Arc<AppState>,
    config: &AutoUpdateConfig,
    metadata: &UpdateMetadata,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 设置更新进行中标志，阻止新任务入队
    state.auto_update_state.set_update_in_progress(true).await;

    tracing::info!(
        current = VERSION,
        latest = %metadata.version,
        "Starting blue-green deployment"
    );

    // 执行蓝绿部署更新
    match download_and_apply_update(state, config, metadata).await {
        Ok(()) => {
            *state.auto_update_state.last_check_result.write().await =
                Some(format!("Successfully updated to {}", metadata.version));
            tracing::info!(version = %metadata.version, "Update applied successfully");
            // 清除 pending_update
            state.auto_update_state.set_pending_update(None).await;
        }
        Err(e) => {
            *state.auto_update_state.last_check_result.write().await =
                Some(format!("Update failed: {}", e));
            tracing::error!(error = %e, "Failed to apply update");
            // 重置状态
            state.auto_update_state.set_update_in_progress(false).await;
            state.auto_update_state.set_pending_update(None).await;
            *state.auto_update_state.update_progress.write().await = "none".to_string();
            return Err(e);
        }
    }

    Ok(())
}

/// 触发待执行的更新（由任务完成时调用）
pub async fn trigger_pending_update(state: Arc<AppState>) {
    let config = match &state.auto_update_config {
        Some(config) => config.clone(),
        None => {
            tracing::warn!("Cannot trigger pending update: auto-update not configured");
            return;
        }
    };

    let metadata = match state.auto_update_state.get_pending_update().await {
        Some(m) => m,
        None => {
            tracing::debug!("No pending update to trigger");
            return;
        }
    };

    // 再次检查是否有任务运行（防止竞态条件）
    let running_deploys = state.running_deploys.read().await;
    if !running_deploys.is_empty() {
        tracing::info!(
            running = running_deploys.len(),
            "Pending update still waiting: tasks still running"
        );
        return;
    }
    drop(running_deploys);

    // 也检查队列是否为空
    let deploy_queue = state.deploy_queue.read().await;
    let total_queued: usize = deploy_queue.values().map(|q| q.len()).sum();
    if total_queued > 0 {
        tracing::info!(
            queued = total_queued,
            "Pending update still waiting: tasks in queue"
        );
        return;
    }
    drop(deploy_queue);

    tracing::info!(
        version = %metadata.version,
        "All tasks complete, triggering deferred update"
    );

    // 执行更新
    if let Err(e) = apply_update_internal(&state, &config, &metadata).await {
        tracing::error!(error = %e, "Failed to apply deferred update");
    }
}

/// 下载并应用更新 (蓝绿部署策略)
async fn download_and_apply_update(
    state: &Arc<AppState>,
    config: &AutoUpdateConfig,
    metadata: &UpdateMetadata,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 尝试通过 Mesh 解析 RustFS 端点，获取最佳下载地址（优先内网）
    let binary_url = resolve_binary_url(state, config, &metadata.version).await;

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
    // 对于 TunnelMode::Client 使用 Takeover 模式（零断线更新）
    // 对于其他模式使用传统重启
    restart_self(state, &current_binary).await?;

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
/// 对于 TunnelMode::Client，使用 Takeover 模式实现零断线更新：
/// 1. 启动新进程（带 --takeover 参数）
/// 2. 新进程连接服务端并发送 Takeover 请求
/// 3. 服务端切换端口绑定到新进程
/// 4. 服务端通知旧进程 TakeoverComplete
/// 5. 旧进程收到消息后退出
///
/// 对于其他模式，使用传统的优雅关闭重启。
async fn restart_self(
    state: &Arc<AppState>,
    binary_path: &PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crate::domain::tunnel::TunnelMode;
    use crate::services::restart::{RestartManager, RestartMode};
    use crate::state::app_state::trigger_shutdown;

    tracing::info!(
        binary = %binary_path.display(),
        tunnel_mode = ?state.tunnel_mode,
        "Restarting process"
    );

    // 对于 TunnelMode::Client，使用 Takeover 模式实现零断线更新
    // 但 Windows 服务模式例外：使用服务恢复机制更可靠
    #[cfg(windows)]
    let use_takeover = {
        use crate::services::windows_service;
        if windows_service::is_running_as_service() {
            tracing::info!("Windows service mode: using service recovery instead of takeover");
            false
        } else {
            state.tunnel_mode == TunnelMode::Client
        }
    };

    #[cfg(not(windows))]
    let use_takeover = state.tunnel_mode == TunnelMode::Client;

    if use_takeover {
        return restart_with_takeover(binary_path).await;
    }

    // 其他模式使用传统重启
    tracing::info!("Using traditional restart (non-client mode)");

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

    // 触发优雅关闭，通知 tunnel client 发送 WebSocket Close 帧
    tracing::info!("Triggering graceful shutdown for tunnel client...");
    trigger_shutdown();

    // 等待 tunnel client 优雅关闭 (发送 Close 帧)
    // 500ms 足够发送 Close 帧并等待服务端确认
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    tracing::info!("Exiting for restart...");
    std::process::exit(0);
}

/// 使用 Takeover 模式重启（零断线更新）
///
/// 流程：
/// 1. 生成唯一的 session ID
/// 2. 启动新进程，带 --takeover <session_id> 参数
/// 3. 新进程连接到服务端并发送 Takeover 请求
/// 4. 服务端切换端口绑定到新进程的 WebSocket 连接
/// 5. 服务端向旧进程发送 TakeoverComplete 消息
/// 6. 旧进程的 tunnel client 收到 TakeoverComplete 后调用 exit(0)
///
/// 本函数不会主动退出，而是等待 TakeoverComplete 触发退出
async fn restart_with_takeover(
    binary_path: &PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 生成唯一的 session ID
    let session_id = uuid::Uuid::new_v4().to_string();

    tracing::info!(
        binary = %binary_path.display(),
        session_id = %session_id,
        "Starting takeover restart (zero-downtime)"
    );

    // 启动新进程，带 --takeover 参数
    let child = Command::new(binary_path)
        .args(["--takeover", &session_id])
        .stdout(Stdio::inherit())  // 继承 stdout 以便查看日志
        .stderr(Stdio::inherit())  // 继承 stderr 以便查看错误
        .spawn();

    match child {
        Ok(mut child) => {
            tracing::info!(
                session_id = %session_id,
                pid = ?child.id(),
                "New process spawned, waiting for takeover"
            );

            // 等待一小段时间让新进程启动
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // 检查新进程是否还在运行
            match child.try_wait() {
                Ok(Some(status)) => {
                    // 新进程已退出，说明启动失败
                    tracing::error!(
                        exit_code = ?status.code(),
                        "New process exited prematurely, takeover failed"
                    );
                    return Err("New process exited prematurely".into());
                }
                Ok(None) => {
                    // 新进程还在运行，继续等待 TakeoverComplete
                    tracing::info!(
                        session_id = %session_id,
                        "New process is running, waiting for TakeoverComplete from server"
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to check new process status");
                    return Err(format!("Failed to check new process: {}", e).into());
                }
            }

            // 此时新进程应该正在连接服务端并发送 Takeover 请求
            // 服务端会向本进程发送 TakeoverComplete 消息
            // tunnel client 收到后会调用 exit(0)
            //
            // 我们在这里等待最多 30 秒，如果还没收到 TakeoverComplete，
            // 说明可能出了问题，但我们不主动退出，让进程继续运行
            tracing::info!(
                "Takeover initiated. Old process will exit when TakeoverComplete is received. \
                 If takeover fails, this process will continue running."
            );

            // 等待 30 秒（给 TakeoverComplete 足够的时间到达）
            // 如果 30 秒后还没退出，说明 TakeoverComplete 没收到
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;

            // 如果执行到这里，说明 30 秒内没有收到 TakeoverComplete
            // 可能是新进程连接失败，或者服务端没有发送消息
            // 检查新进程状态
            match child.try_wait() {
                Ok(Some(status)) => {
                    // 新进程已退出，说明 takeover 失败
                    tracing::warn!(
                        exit_code = ?status.code(),
                        "New process has exited. Takeover failed. Old process continues."
                    );
                    // 新进程挂了，旧进程继续运行
                    return Ok(());
                }
                Ok(None) => {
                    // 新进程还在运行，说明 takeover 可能成功了
                    // 旧进程应该退出，避免僵尸进程
                    tracing::info!(
                        "30 seconds passed, new process is still running. \
                         Assuming takeover succeeded. Exiting old process."
                    );
                    std::process::exit(0);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to check new process status after timeout");
                    // 无法确定状态，保守起见退出
                    std::process::exit(0);
                }
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to spawn new process for takeover");
            Err(format!("Failed to spawn new process: {}", e).into())
        }
    }
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

/// 通过 Mesh 解析最佳二进制下载 URL
///
/// 如果 Mesh 客户端可用且能解析 "rustfs" 服务，则使用解析到的端点 URL（可能是内网地址）；
/// 否则使用配置中的默认端点 URL。
async fn resolve_binary_url(state: &Arc<AppState>, config: &AutoUpdateConfig, version: &str) -> String {
    // 构建二进制路径（不包含端点）
    let binary_path = config.binary_path_template.replace("{version}", version);

    // 尝试通过 Mesh 解析 RustFS 服务端点
    if let Some(ref mesh_client) = state.mesh_client {
        if mesh_client.is_enabled() {
            match mesh_client.get_service_url("rustfs").await {
                Ok(endpoint_url) => {
                    let resolved_url = format!("{}/{}", endpoint_url.trim_end_matches('/'), binary_path);
                    tracing::info!(
                        original_endpoint = %config.endpoint,
                        resolved_endpoint = %endpoint_url,
                        resolved_url = %resolved_url,
                        "Resolved RustFS endpoint via Mesh (using LAN if available)"
                    );
                    return resolved_url;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        fallback_endpoint = %config.endpoint,
                        "Failed to resolve RustFS via Mesh, using fallback"
                    );
                }
            }
        }
    }

    // Fallback: 使用配置中的默认端点
    config.binary_url(version)
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
