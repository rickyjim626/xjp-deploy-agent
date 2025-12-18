//! Docker Build deployment execution
//!
//! Handles Docker Build deployments (pulling code, building images, pushing to registry)

use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::domain::deploy::{DeployStage, DeployStatus, StageStatus};

use super::context::DeployContext;

/// Configuration for Docker Build deployment
pub struct DockerBuildConfig {
    pub dockerfile: Option<String>,
    pub build_context: Option<String>,
    pub image: String,
    pub tag: Option<String>,
    pub push_latest: bool,
    pub branch: Option<String>,
}

/// Execute a Docker Build deployment
pub async fn execute(ctx: &DeployContext, work_dir: &str, config: DockerBuildConfig) {
    // Initialize stages
    let mut stages = vec![
        DeployStage::new("git_pull", "Git Pull"),
        DeployStage::new("docker_build", "Docker Build"),
        DeployStage::new("docker_push", "Docker Push"),
    ];
    if config.push_latest {
        stages.push(DeployStage::new("docker_push_latest", "Push Latest Tag"));
    }

    let dockerfile_path = config.dockerfile.as_deref().unwrap_or("Dockerfile");
    let context_path = config.build_context.as_deref().unwrap_or(".");
    let branch_name = config.branch.as_deref().unwrap_or("master");

    ctx.log_stdout(&format!("=== Docker Build Deploy for {} ===", ctx.project))
        .await;
    ctx.log_stdout(&format!("Working directory: {}", work_dir))
        .await;
    ctx.log_stdout(&format!("Dockerfile: {}", dockerfile_path))
        .await;
    ctx.log_stdout(&format!("Build context: {}", context_path))
        .await;
    ctx.log_stdout(&format!("Target image: {}", config.image))
        .await;
    ctx.log_stdout(&format!("Branch: {}", branch_name)).await;
    ctx.log_stdout(&format!(
        "Timestamp: {}",
        chrono::Utc::now().to_rfc3339()
    ))
    .await;

    let mut exit_code = 0;
    let mut commit_hash = config.tag.clone().unwrap_or_else(|| "latest".to_string());

    // Stage 1: Git Pull
    stages[0].start();
    ctx.update_stages(stages.clone()).await;
    ctx.log_stdout("[1/4] Pulling latest code...").await;
    ctx.log_stdout(&format!(
        ">>> git pull --ff-only origin {}",
        branch_name
    ))
    .await;

    let git_result = Command::new("git")
        .args(["pull", "--ff-only", "origin", branch_name])
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
                stages[0].finish(false, Some("git pull failed".to_string()));
                exit_code = output.status.code().unwrap_or(-1);
            }
        }
        Err(e) => {
            ctx.log_stderr(&format!("Error: Failed to run git pull: {}", e))
                .await;
            stages[0].finish(false, Some(e.to_string()));
            exit_code = -1;
        }
    }
    ctx.update_stages(stages.clone()).await;

    // Get commit hash as tag if not specified
    if exit_code == 0 && config.tag.is_none() {
        let hash_result = Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(work_dir)
            .output()
            .await;

        if let Ok(output) = hash_result {
            if output.status.success() {
                commit_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
                ctx.log_stdout(&format!("Using commit hash as tag: {}", commit_hash))
                    .await;
            }
        }
    }

    // Check cancellation
    if ctx.is_cancelled() {
        ctx.log_stderr("Deployment cancelled").await;
        ctx.finish(DeployStatus::Failed, Some(-2), stages).await;
        return;
    }

    // Stage 2: Docker Build
    if exit_code == 0 {
        // Clean up zombie docker processes
        ctx.log_stdout("Checking for zombie docker processes...")
            .await;
        let (zombies, hung) = cleanup_zombie_docker_processes().await;
        if zombies > 0 || hung > 0 {
            ctx.log_stdout(&format!(
                "Cleaned up {} zombie and {} hung docker processes",
                zombies, hung
            ))
            .await;
        }

        stages[1].start();
        ctx.update_stages(stages.clone()).await;

        let full_image = format!("{}:{}", config.image, commit_hash);
        ctx.log_stdout("[2/4] Building Docker image...").await;
        ctx.log_stdout(&format!(
            ">>> docker build -t {} -f {} {}",
            full_image, dockerfile_path, context_path
        ))
        .await;

        let build_result = Command::new("docker")
            .args([
                "build",
                "--progress=plain",
                "-t",
                &full_image,
                "-f",
                dockerfile_path,
                context_path,
            ])
            .current_dir(work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        match build_result {
            Ok(mut child) => {
                let stderr = child.stderr.take();
                let stdout = child.stdout.take();

                // Spawn tasks to stream output
                let ctx_stderr = ctx.clone();
                let stderr_task = tokio::spawn(async move {
                    if let Some(stderr) = stderr {
                        let reader = BufReader::new(stderr);
                        let mut lines = reader.lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            ctx_stderr.log_stderr(&line).await;
                        }
                    }
                });

                let ctx_stdout = ctx.clone();
                let stdout_task = tokio::spawn(async move {
                    if let Some(stdout) = stdout {
                        let reader = BufReader::new(stdout);
                        let mut lines = reader.lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            ctx_stdout.log_stdout(&line).await;
                        }
                    }
                });

                // Wait for build with cancellation support
                let build_result = tokio::select! {
                    _ = ctx.cancel_token.cancelled() => {
                        tracing::warn!(task_id = %ctx.task_id, project = %ctx.project, "Docker build cancelled");
                        ctx.log_stderr("=== Docker build CANCELLED: superseded by newer commit ===").await;

                        // Kill build process
                        if let Err(e) = child.kill().await {
                            tracing::warn!(task_id = %ctx.task_id, error = %e, "Failed to kill docker build process");
                        }

                        // Abort output tasks
                        stderr_task.abort();
                        stdout_task.abort();

                        // Mark all pending stages as skipped
                        for stage in stages.iter_mut() {
                            if matches!(stage.status, StageStatus::Pending | StageStatus::Running) {
                                stage.status = StageStatus::Skipped;
                            }
                        }

                        ctx.log_stdout("=== Docker Build Deploy cancelled ===").await;
                        ctx.finish(DeployStatus::Failed, Some(-2), stages).await;
                        return;
                    }

                    result = child.wait() => {
                        // Wait for output tasks
                        let _ = tokio::join!(stderr_task, stdout_task);
                        result
                    }
                };

                match build_result {
                    Ok(exit_status) => {
                        if exit_status.success() {
                            stages[1].finish(true, None);
                            ctx.log_stdout(&format!("✓ Image built: {}", full_image))
                                .await;
                        } else {
                            stages[1].finish(false, Some("docker build failed".to_string()));
                            exit_code = exit_status.code().unwrap_or(-1);
                        }
                    }
                    Err(e) => {
                        stages[1].finish(false, Some(e.to_string()));
                        ctx.log_stderr(&format!("Error waiting for build: {}", e))
                            .await;
                        exit_code = -1;
                    }
                }
            }
            Err(e) => {
                stages[1].finish(false, Some(e.to_string()));
                ctx.log_stderr(&format!("Error: Failed to start docker build: {}", e))
                    .await;
                exit_code = -1;
            }
        }
        ctx.update_stages(stages.clone()).await;
    }

    // Stage 3: Docker Push
    if exit_code == 0 && ctx.is_cancelled() {
        ctx.log_stderr("=== Deployment cancelled before push ===")
            .await;
        ctx.finish(DeployStatus::Failed, Some(-2), stages).await;
        return;
    }

    if exit_code == 0 {
        stages[2].start();
        ctx.update_stages(stages.clone()).await;

        let full_image = format!("{}:{}", config.image, commit_hash);
        ctx.log_stdout("[3/4] Pushing image to registry...").await;
        ctx.log_stdout(&format!(">>> docker push {}", full_image))
            .await;

        let push_result = Command::new("docker")
            .args(["push", &full_image])
            .output()
            .await;

        match push_result {
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
                    stages[2].finish(true, None);
                    ctx.log_stdout(&format!("✓ Pushed: {}", full_image)).await;
                } else {
                    stages[2].finish(false, Some("docker push failed".to_string()));
                    exit_code = output.status.code().unwrap_or(-1);
                }
            }
            Err(e) => {
                stages[2].finish(false, Some(e.to_string()));
                ctx.log_stderr(&format!("Error: Failed to push image: {}", e))
                    .await;
                exit_code = -1;
            }
        }
        ctx.update_stages(stages.clone()).await;
    }

    // Stage 4: Push Latest Tag (optional)
    if exit_code == 0 && ctx.is_cancelled() {
        ctx.log_stderr("=== Deployment cancelled before push latest ===")
            .await;
        ctx.finish(DeployStatus::Failed, Some(-2), stages).await;
        return;
    }

    if exit_code == 0 && config.push_latest && stages.len() > 3 {
        stages[3].start();
        ctx.update_stages(stages.clone()).await;

        let full_image = format!("{}:{}", config.image, commit_hash);
        let latest_image = format!("{}:latest", config.image);
        ctx.log_stdout("[4/4] Tagging and pushing latest...").await;

        // Tag as latest
        ctx.log_stdout(&format!(">>> docker tag {} {}", full_image, latest_image))
            .await;
        let tag_result = Command::new("docker")
            .args(["tag", &full_image, &latest_image])
            .output()
            .await;

        if let Ok(output) = tag_result {
            if !output.status.success() {
                ctx.log_stderr("Warning: Failed to tag as latest").await;
            }
        }

        // Push latest
        ctx.log_stdout(&format!(">>> docker push {}", latest_image))
            .await;
        let push_latest_result = Command::new("docker")
            .args(["push", &latest_image])
            .output()
            .await;

        match push_latest_result {
            Ok(output) => {
                if !output.stdout.is_empty() {
                    ctx.log_stdout(&String::from_utf8_lossy(&output.stdout))
                        .await;
                }
                if output.status.success() {
                    stages[3].finish(true, None);
                    ctx.log_stdout(&format!("✓ Pushed: {}", latest_image)).await;
                } else {
                    stages[3].finish(false, Some("push latest failed".to_string()));
                    // Don't fail the whole deploy for latest tag
                    ctx.log_stderr("Warning: Failed to push latest tag").await;
                }
            }
            Err(e) => {
                stages[3].finish(false, Some(e.to_string()));
                ctx.log_stderr(&format!("Warning: Failed to push latest: {}", e))
                    .await;
            }
        }
        ctx.update_stages(stages.clone()).await;
    }

    // Finish
    ctx.log_stdout("=== Docker Build Deploy finished ===").await;
    let status = if exit_code == 0 {
        DeployStatus::Success
    } else {
        DeployStatus::Failed
    };
    ctx.log_stdout(&format!("Status: {:?}", status)).await;
    ctx.log_stdout(&format!("Exit code: {}", exit_code)).await;

    // Print stage summary
    ctx.log_stdout("\n=== Stage Summary ===").await;
    for stage in &stages {
        let duration = stage
            .duration_ms
            .map(|d| format!("{}ms", d))
            .unwrap_or_else(|| "-".to_string());
        let status_icon = match stage.status {
            StageStatus::Success => "✓",
            StageStatus::Failed => "✗",
            StageStatus::Skipped => "⊘",
            StageStatus::Running => "⟳",
            StageStatus::Pending => "○",
        };
        ctx.log_stdout(&format!(
            "{} {} ({})",
            status_icon, stage.display_name, duration
        ))
        .await;
    }

    ctx.finish(status, Some(exit_code), stages).await;

    tracing::info!(
        task_id = %ctx.task_id,
        project = %ctx.project,
        exit_code = exit_code,
        "Docker Build deployment finished"
    );
}

/// Clean up zombie docker processes and hung builds
async fn cleanup_zombie_docker_processes() -> (usize, usize) {
    let mut zombies_killed = 0;
    let mut hung_builds_killed = 0;

    // 1. Check for zombie processes (state Z or <defunct>)
    if let Ok(output) = Command::new("ps").args(["aux"]).output().await {
        let ps_output = String::from_utf8_lossy(&output.stdout);
        for line in ps_output.lines() {
            if (line.contains("docker") || line.contains("buildkit"))
                && (line.contains("<defunct>") || line.contains(" Z ") || line.contains(" Z+ "))
            {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(pid) = parts[1].parse::<i32>() {
                        tracing::info!("Found zombie docker process: PID={}, killing...", pid);
                        if Command::new("kill")
                            .args(["-9", &pid.to_string()])
                            .output()
                            .await
                            .is_ok()
                        {
                            zombies_killed += 1;
                        }
                    }
                }
            }
        }
    }

    // 2. Check for long-running docker build processes (> 30 min)
    if let Ok(output) = Command::new("ps")
        .args(["-eo", "pid,etimes,args"])
        .output()
        .await
    {
        let ps_output = String::from_utf8_lossy(&output.stdout);
        for line in ps_output.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let pid = parts[0];
                let elapsed_secs: i64 = parts[1].parse().unwrap_or(0);
                let cmd = parts[2..].join(" ");

                // More than 30 minutes (1800 seconds)
                if elapsed_secs > 1800
                    && (cmd.contains("docker build") || cmd.contains("docker-buildx"))
                {
                    tracing::info!(
                        "Found hung docker build process: PID={}, running for {}s, killing...",
                        pid,
                        elapsed_secs
                    );
                    if Command::new("kill")
                        .args(["-9", pid])
                        .output()
                        .await
                        .is_ok()
                    {
                        hung_builds_killed += 1;
                    }
                }
            }
        }
    }

    // 3. Clean up orphan buildkit processes
    if let Ok(output) = Command::new("pgrep")
        .args(["-f", "buildkit"])
        .output()
        .await
    {
        if output.status.success() {
            let pids = String::from_utf8_lossy(&output.stdout);
            for pid in pids.lines() {
                if !pid.trim().is_empty() {
                    // Check if parent is init (PID 1) = orphan process
                    if let Ok(ppid_output) = Command::new("ps")
                        .args(["-o", "ppid=", "-p", pid.trim()])
                        .output()
                        .await
                    {
                        let ppid = String::from_utf8_lossy(&ppid_output.stdout)
                            .trim()
                            .to_string();
                        if ppid == "1" {
                            tracing::info!(
                                "Found orphan buildkit process: PID={}, killing...",
                                pid.trim()
                            );
                            let _ = Command::new("kill")
                                .args(["-9", pid.trim()])
                                .output()
                                .await;
                            hung_builds_killed += 1;
                        }
                    }
                }
            }
        }
    }

    (zombies_killed, hung_builds_killed)
}
