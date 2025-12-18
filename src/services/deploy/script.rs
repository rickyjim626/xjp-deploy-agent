//! Script deployment execution
//!
//! Handles script-based deployments (execute_command type)

use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::domain::deploy::{DeployStage, DeployStatus};

use super::context::DeployContext;

/// Execute a script-based deployment
pub async fn execute(ctx: &DeployContext, work_dir: &str, script: &str) {
    // Initialize stages
    let mut stages = vec![
        DeployStage::new("git_pull", "Git Pull"),
        DeployStage::new("execute_script", "Execute Script"),
    ];

    ctx.log_stdout(&format!("=== Starting deployment for {} ===", ctx.project))
        .await;
    ctx.log_stdout(&format!("Working directory: {}", work_dir))
        .await;
    ctx.log_stdout(&format!("Script: {}", script)).await;

    if let Some(ref hash) = ctx.commit_hash {
        ctx.log_stdout(&format!("Commit: {}", hash)).await;
    }
    if let Some(ref br) = ctx.branch {
        ctx.log_stdout(&format!("Branch: {}", br)).await;
    }

    #[allow(unused_assignments)]
    let mut exit_code = 0;

    // Stage 1: Git pull
    stages[0].start();
    ctx.update_stages(stages.clone()).await;
    ctx.log_stdout(">>> git pull").await;

    let git_result = Command::new("git")
        .arg("pull")
        .current_dir(work_dir)
        .output()
        .await;

    match git_result {
        Ok(output) => {
            if !output.stdout.is_empty() {
                ctx.log_stdout(&String::from_utf8_lossy(&output.stdout))
                    .await;
            }
            if !output.stderr.is_empty() {
                ctx.log_stderr(&String::from_utf8_lossy(&output.stderr))
                    .await;
            }
            if output.status.success() {
                stages[0].finish(true, None);
            } else {
                stages[0].finish(false, Some("git pull failed, continuing anyway".to_string()));
                ctx.log_stderr("git pull failed, continuing anyway...")
                    .await;
            }
        }
        Err(e) => {
            stages[0].finish(false, Some(e.to_string()));
            ctx.log_stderr(&format!("Failed to run git pull: {}", e))
                .await;
        }
    }
    ctx.update_stages(stages.clone()).await;

    // Check cancellation
    if ctx.is_cancelled() {
        ctx.log_stderr("=== Deployment CANCELLED ===").await;
        ctx.finish(DeployStatus::Failed, Some(-2), stages).await;
        return;
    }

    // Stage 2: Execute script
    stages[1].start();
    ctx.update_stages(stages.clone()).await;
    ctx.log_stdout(&format!(">>> {}", script)).await;

    let mut child = match Command::new("bash")
        .arg("-c")
        .arg(script)
        .current_dir(work_dir)
        .env("COMMIT_HASH", ctx.commit_hash.as_deref().unwrap_or(""))
        .env("BRANCH", ctx.branch.as_deref().unwrap_or(""))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            tracing::error!(task_id = %ctx.task_id, error = %e, "Failed to spawn process");
            ctx.log_stderr(&format!("Failed to start script: {}", e))
                .await;
            stages[1].finish(false, Some(e.to_string()));
            ctx.finish(DeployStatus::Failed, Some(-1), stages).await;
            return;
        }
    };

    // Stream stdout/stderr
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let ctx_stdout = ctx.clone();
    let ctx_stderr = ctx.clone();

    let stdout_task = tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if should_send_to_center(&line) {
                ctx_stdout.log_stdout(&line).await;
            }
        }
    });

    let stderr_task = tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if should_send_to_center(&line) {
                ctx_stderr.log_stderr(&line).await;
            }
        }
    });

    // Wait for process completion with cancellation support
    let (status, code) = tokio::select! {
        _ = ctx.cancel_token.cancelled() => {
            tracing::warn!(task_id = %ctx.task_id, project = %ctx.project, "Deployment cancelled");
            ctx.log_stderr("=== Deployment CANCELLED ===").await;

            // Kill child process
            if let Err(e) = child.kill().await {
                tracing::warn!(task_id = %ctx.task_id, error = %e, "Failed to kill child process");
            }

            // Abort output tasks
            stdout_task.abort();
            stderr_task.abort();

            stages[1].finish(false, Some("cancelled".to_string()));
            ctx.finish(DeployStatus::Failed, Some(-2), stages).await;
            return;
        }

        exit_status = child.wait() => {
            // Wait for output tasks to complete
            let _ = stdout_task.await;
            let _ = stderr_task.await;

            match exit_status {
                Ok(status) => {
                    let code = status.code().unwrap_or(-1);
                    if status.success() {
                        ctx.log_stdout(&format!("=== Deployment completed successfully (exit code: {}) ===", code)).await;
                        (DeployStatus::Success, code)
                    } else {
                        ctx.log_stderr(&format!("=== Deployment failed (exit code: {}) ===", code)).await;
                        (DeployStatus::Failed, code)
                    }
                }
                Err(e) => {
                    ctx.log_stderr(&format!("=== Process error: {} ===", e)).await;
                    (DeployStatus::Failed, -1)
                }
            }
        }
    };

    exit_code = code;
    stages[1].finish(status == DeployStatus::Success, None);

    // Finish deployment
    ctx.finish(status, Some(exit_code), stages).await;

    tracing::info!(
        task_id = %ctx.task_id,
        project = %ctx.project,
        exit_code = exit_code,
        "Script deployment finished"
    );
}

/// Filter unimportant log lines for deploy center
fn should_send_to_center(line: &str) -> bool {
    let line = line.trim();
    // Filter empty lines
    if line.is_empty() {
        return false;
    }
    // Docker buildx output - more lenient filtering
    if line.starts_with('#')
        && line
            .chars()
            .nth(1)
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
    {
        return line.contains("DONE")
            || line.contains("CACHED")
            || line.contains("ERROR")
            || line.contains("error:")
            || line.contains("warning:")
            || line.contains("Compiling")
            || line.contains("Building")
            || line.contains("Downloading")
            || line.contains("Downloaded")
            || line.contains("[stage")
            || line.contains("[builder")
            || line.contains("FROM")
            || line.contains("RUN")
            || line.contains("COPY")
            || line.contains("exporting");
    }
    // Filter redundant sha256 lines
    if line.contains("sha256:")
        && (line.contains("resolve") || line.contains("Pulling") || line.contains("Waiting"))
    {
        return false;
    }
    // Filter buildx progress bars
    if line.contains("[>") || line.contains("[ ") || line.contains("[=") {
        return false;
    }
    true
}
