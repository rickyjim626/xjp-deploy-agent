//! Xiaojincut task execution
//!
//! Handles Xiaojincut local video processing tasks

use std::time::Duration;

use crate::domain::deploy::DeployStatus;

use super::context::DeployContext;

/// Configuration for Xiaojincut task
pub struct XiaojincutConfig {
    /// Task type: "import", "export", "process", "status", "timeline"
    pub task_type: String,
    /// Input file path
    pub input_path: Option<String>,
    /// Output directory
    pub output_dir: Option<String>,
    /// AI prompt (for AI processing tasks)
    pub prompt: Option<String>,
    /// Xiaojincut service port (default 19527)
    pub port: Option<u16>,
}

/// Execute a Xiaojincut task
pub async fn execute(ctx: &DeployContext, config: XiaojincutConfig) {
    let port = config.port.unwrap_or(19527);
    let base_url = format!("http://localhost:{}/api", port);
    let http_client = reqwest::Client::new();

    ctx.log_stdout("═══════════════════════════════════════════════════════════")
        .await;
    ctx.log_stdout(&format!("Xiaojincut Task: {}", config.task_type))
        .await;
    ctx.log_stdout("═══════════════════════════════════════════════════════════")
        .await;
    ctx.log_stdout(&format!("Target: http://localhost:{}", port))
        .await;

    if let Some(ref path) = config.input_path {
        ctx.log_stdout(&format!("Input: {}", path)).await;
    }
    if let Some(ref dir) = config.output_dir {
        ctx.log_stdout(&format!("Output Dir: {}", dir)).await;
    }
    if let Some(ref p) = config.prompt {
        ctx.log_stdout(&format!("Prompt: {}", p)).await;
    }
    ctx.log_stdout("───────────────────────────────────────────────────────────")
        .await;

    // Check if xiaojincut service is running
    ctx.log_stdout(">>> Checking xiaojincut service health...")
        .await;

    let health_url = format!("{}/health", base_url);
    let health_result = tokio::select! {
        _ = ctx.cancel_token.cancelled() => {
            ctx.log_stderr("Task cancelled").await;
            ctx.finish(DeployStatus::Failed, Some(-1), vec![]).await;
            return;
        }
        result = http_client.get(&health_url).timeout(Duration::from_secs(5)).send() => result
    };

    match health_result {
        Ok(resp) if resp.status().is_success() => {
            ctx.log_stdout("✅ xiaojincut service is running").await;
        }
        Ok(resp) => {
            ctx.log_stderr(&format!(
                "❌ xiaojincut health check failed: HTTP {}",
                resp.status()
            ))
            .await;
            ctx.finish(DeployStatus::Failed, Some(1), vec![]).await;
            return;
        }
        Err(e) => {
            ctx.log_stderr(&format!("❌ xiaojincut service not reachable: {}", e))
                .await;
            ctx.log_stderr("Tip: Start xiaojincut with: open /Applications/xiaojincut.app")
                .await;
            ctx.finish(DeployStatus::Failed, Some(1), vec![]).await;
            return;
        }
    }

    // Execute task based on type
    let result = match config.task_type.as_str() {
        "import" => {
            if let Some(path) = config.input_path {
                ctx.log_stdout(&format!(">>> Importing media: {}", path))
                    .await;
                let import_url = format!("{}/import", base_url);
                http_client
                    .post(&import_url)
                    .json(&serde_json::json!({ "path": path }))
                    .timeout(Duration::from_secs(60))
                    .send()
                    .await
            } else {
                ctx.log_stderr("❌ Missing input_path for import task")
                    .await;
                ctx.finish(DeployStatus::Failed, Some(1), vec![]).await;
                return;
            }
        }
        "export" => {
            ctx.log_stdout(">>> Exporting timeline...").await;
            let export_url = format!("{}/export", base_url);
            let mut payload = serde_json::json!({});
            if let Some(dir) = config.output_dir {
                payload["output_dir"] = serde_json::Value::String(dir);
            }
            http_client
                .post(&export_url)
                .json(&payload)
                .timeout(Duration::from_secs(300))
                .send()
                .await
        }
        "status" => {
            ctx.log_stdout(">>> Getting status...").await;
            let status_url = format!("{}/status", base_url);
            http_client
                .get(&status_url)
                .timeout(Duration::from_secs(10))
                .send()
                .await
        }
        "timeline" => {
            ctx.log_stdout(">>> Getting timeline...").await;
            let timeline_url = format!("{}/timeline", base_url);
            http_client
                .get(&timeline_url)
                .timeout(Duration::from_secs(10))
                .send()
                .await
        }
        _ => {
            ctx.log_stderr(&format!("❌ Unknown task type: {}", config.task_type))
                .await;
            ctx.finish(DeployStatus::Failed, Some(1), vec![]).await;
            return;
        }
    };

    // Process response
    match result {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();

            if status.is_success() {
                ctx.log_stdout("───────────────────────────────────────────────────────────")
                    .await;
                ctx.log_stdout("✅ Task completed successfully").await;

                // Format JSON output
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                    let pretty = serde_json::to_string_pretty(&json).unwrap_or(body.clone());
                    for line in pretty.lines() {
                        ctx.log_stdout(line).await;
                    }
                } else if !body.is_empty() {
                    ctx.log_stdout(&body).await;
                }

                ctx.log_stdout("═══════════════════════════════════════════════════════════")
                    .await;
                ctx.finish(DeployStatus::Success, Some(0), vec![]).await;
            } else {
                ctx.log_stderr(&format!("❌ Task failed: HTTP {}", status))
                    .await;
                if !body.is_empty() {
                    ctx.log_stderr(&body).await;
                }
                ctx.finish(DeployStatus::Failed, Some(1), vec![]).await;
            }
        }
        Err(e) => {
            ctx.log_stderr(&format!("❌ Request failed: {}", e)).await;
            ctx.finish(DeployStatus::Failed, Some(1), vec![]).await;
        }
    }
}
