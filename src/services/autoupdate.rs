//! 自动更新服务
//!
//! 定期检查更新并自动应用

use std::sync::Arc;

use crate::config::autoupdate::UpdateMetadata;
use crate::config::env::constants::VERSION;
use crate::state::AppState;

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
        "Starting auto-update task"
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

/// 检查并应用更新
async fn check_and_apply_update(
    state: &Arc<AppState>,
    config: &crate::config::autoupdate::AutoUpdateConfig,
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
        "Update available"
    );

    // TODO: 下载并应用更新
    // download_and_apply_update(state, config, &metadata).await?;

    *state.auto_update_state.last_check_result.write().await =
        Some(format!("Update available: {}", metadata.version));

    Ok(())
}

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
