//! Docker Compose deployment execution
//!
//! Handles Docker Compose deployments (pulling images and restarting containers)

use tokio::process::Command;

use crate::domain::deploy::{DeployStage, DeployStatus, StageStatus};

use super::context::DeployContext;

/// Configuration for Docker Compose deployment
pub struct DockerComposeConfig {
    pub compose_file: String,
    pub image: String,
    pub git_repo_dir: Option<String>,
    pub service: Option<String>,
}

/// Execute a Docker Compose deployment
pub async fn execute(ctx: &DeployContext, work_dir: &str, config: DockerComposeConfig) {
    // Initialize stages
    let mut stages = vec![
        DeployStage::new("git_pull", "Git Pull"),
        DeployStage::new("docker_pull", "Docker Pull"),
        DeployStage::new("compose_pull", "Compose Pull"),
        DeployStage::new("compose_up", "Compose Up"),
    ];

    ctx.log_stdout(&format!(
        "=== Docker Compose Deploy for {} ===",
        ctx.project
    ))
    .await;
    ctx.log_stdout(&format!("Working directory: {}", work_dir))
        .await;
    ctx.log_stdout(&format!("Compose file: {}", config.compose_file))
        .await;
    ctx.log_stdout(&format!("Image: {}", config.image)).await;
    ctx.log_stdout(&format!(
        "Timestamp: {}",
        chrono::Utc::now().to_rfc3339()
    ))
    .await;

    let mut exit_code = 0;

    // Build full compose file path
    let compose_path = if config.compose_file.starts_with('/') {
        config.compose_file.clone()
    } else {
        format!("{}/{}", work_dir, config.compose_file)
    };

    // Detect docker-compose command (prefer docker-compose, fallback to docker compose)
    let (docker_compose_cmd, docker_compose_args) = detect_compose_command().await;
    ctx.log_stdout(&format!(
        "Using: {} {:?}",
        docker_compose_cmd, docker_compose_args
    ))
    .await;

    // Stage 1: Git pull (if configured)
    stages[0].start();
    ctx.update_stages(stages.clone()).await;

    if let Some(ref git_dir) = config.git_repo_dir {
        ctx.log_stdout("[1/4] Updating repository...").await;
        ctx.log_stdout(&format!(">>> git pull (in {})", git_dir))
            .await;

        let git_result = Command::new("git")
            .args(["pull", "--ff-only", "origin", "master"])
            .current_dir(git_dir)
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
                    stages[0].finish(false, Some("git pull failed, continuing".to_string()));
                    ctx.log_stderr("Warning: git pull failed, continuing with existing files")
                        .await;
                }
            }
            Err(e) => {
                stages[0].finish(false, Some(e.to_string()));
                ctx.log_stderr(&format!("Warning: Failed to run git pull: {}", e))
                    .await;
            }
        }
    } else {
        stages[0].skip(Some("not configured".to_string()));
        ctx.log_stdout("[1/4] Skipping git pull (not configured)")
            .await;
    }
    ctx.update_stages(stages.clone()).await;

    // Check cancellation
    if ctx.is_cancelled() {
        ctx.log_stderr("Deployment cancelled").await;
        ctx.finish(DeployStatus::Failed, Some(-2), stages).await;
        return;
    }

    // Stage 2: Docker pull image
    if exit_code == 0 {
        stages[1].start();
        ctx.update_stages(stages.clone()).await;
        ctx.log_stdout("[2/4] Pulling Docker image...").await;
        ctx.log_stdout(&format!(">>> docker pull {}", config.image))
            .await;

        let pull_result = Command::new("docker")
            .args(["pull", &config.image])
            .output()
            .await;

        match pull_result {
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
                    stages[1].finish(true, None);
                } else {
                    stages[1].finish(false, Some("docker pull failed".to_string()));
                    ctx.log_stderr("Error: Failed to pull image").await;
                    exit_code = output.status.code().unwrap_or(-1);
                }
            }
            Err(e) => {
                stages[1].finish(false, Some(e.to_string()));
                ctx.log_stderr(&format!("Error: Failed to run docker pull: {}", e))
                    .await;
                exit_code = -1;
            }
        }
        ctx.update_stages(stages.clone()).await;
    }

    // Check cancellation
    if ctx.is_cancelled() && exit_code == 0 {
        ctx.log_stderr("Deployment cancelled").await;
        ctx.finish(DeployStatus::Failed, Some(-2), stages).await;
        return;
    }

    // Stage 3: Docker compose pull
    if exit_code == 0 {
        stages[2].start();
        ctx.update_stages(stages.clone()).await;
        let service_str = config.service.as_deref().unwrap_or("all services");
        ctx.log_stdout(&format!("[3/4] Pulling {}...", service_str))
            .await;

        let mut args = docker_compose_args.clone();
        args.extend(["-f", &compose_path, "pull"]);
        if let Some(ref svc) = config.service {
            args.push(svc.as_str());
            ctx.log_stdout(&format!(
                ">>> {} -f {} pull {}",
                docker_compose_cmd, compose_path, svc
            ))
            .await;
        } else {
            ctx.log_stdout(&format!(
                ">>> {} -f {} pull",
                docker_compose_cmd, compose_path
            ))
            .await;
        }

        let compose_pull = Command::new(docker_compose_cmd)
            .args(&args)
            .current_dir(work_dir)
            .output()
            .await;

        match compose_pull {
            Ok(output) => {
                if !output.stdout.is_empty() {
                    ctx.log_stdout(&String::from_utf8_lossy(&output.stdout))
                        .await;
                }
                if !output.stderr.is_empty() {
                    // compose pull progress info usually goes to stderr
                    ctx.log_stdout(&String::from_utf8_lossy(&output.stderr))
                        .await;
                }
                if output.status.success() {
                    stages[2].finish(true, None);
                } else {
                    stages[2].finish(false, Some("compose pull had issues".to_string()));
                    ctx.log_stderr("Warning: compose pull had issues, continuing...")
                        .await;
                }
            }
            Err(e) => {
                stages[2].finish(false, Some(e.to_string()));
                ctx.log_stderr(&format!("Warning: Failed to run compose pull: {}", e))
                    .await;
            }
        }
        ctx.update_stages(stages.clone()).await;
    }

    // Stage 4: Docker compose up -d --force-recreate
    if exit_code == 0 {
        stages[3].start();
        ctx.update_stages(stages.clone()).await;
        let service_str = config.service.as_deref().unwrap_or("all services");
        ctx.log_stdout(&format!("[4/4] Restarting {}...", service_str))
            .await;

        let mut args = docker_compose_args.clone();
        args.extend(["-f", &compose_path, "up", "-d", "--force-recreate"]);
        if let Some(ref svc) = config.service {
            args.push(svc.as_str());
            ctx.log_stdout(&format!(
                ">>> {} -f {} up -d --force-recreate {}",
                docker_compose_cmd, compose_path, svc
            ))
            .await;
        } else {
            ctx.log_stdout(&format!(
                ">>> {} -f {} up -d --force-recreate",
                docker_compose_cmd, compose_path
            ))
            .await;
        }

        let compose_up = Command::new(docker_compose_cmd)
            .args(&args)
            .current_dir(work_dir)
            .output()
            .await;

        match compose_up {
            Ok(output) => {
                if !output.stdout.is_empty() {
                    ctx.log_stdout(&String::from_utf8_lossy(&output.stdout))
                        .await;
                }
                if !output.stderr.is_empty() {
                    ctx.log_stdout(&String::from_utf8_lossy(&output.stderr))
                        .await;
                }
                if output.status.success() {
                    stages[3].finish(true, None);
                } else {
                    stages[3].finish(false, Some("compose up failed".to_string()));
                    ctx.log_stderr("Error: Failed to start services").await;
                    exit_code = output.status.code().unwrap_or(-1);
                }
            }
            Err(e) => {
                stages[3].finish(false, Some(e.to_string()));
                ctx.log_stderr(&format!("Error: Failed to run compose up: {}", e))
                    .await;
                exit_code = -1;
            }
        }
        ctx.update_stages(stages.clone()).await;
    }

    // Show final status
    if exit_code == 0 {
        ctx.log_stdout("").await;
        ctx.log_stdout("=== Deployment Complete ===").await;

        // Show container status
        let mut args = docker_compose_args.clone();
        args.extend(["-f", &compose_path, "ps"]);

        if let Ok(output) = Command::new(docker_compose_cmd)
            .args(&args)
            .current_dir(work_dir)
            .output()
            .await
        {
            if !output.stdout.is_empty() {
                ctx.log_stdout(&String::from_utf8_lossy(&output.stdout))
                    .await;
            }
        }

        // Show last few log lines
        ctx.log_stdout("").await;
        ctx.log_stdout("Container logs (last 10 lines):").await;

        let mut args = docker_compose_args.clone();
        args.extend(["-f", &compose_path, "logs", "--tail=10"]);

        if let Ok(output) = Command::new(docker_compose_cmd)
            .args(&args)
            .current_dir(work_dir)
            .output()
            .await
        {
            if !output.stdout.is_empty() {
                ctx.log_stdout(&String::from_utf8_lossy(&output.stdout))
                    .await;
            }
        }
    }

    // Print stage summary
    ctx.log_stdout("").await;
    ctx.log_stdout("=== Stage Summary ===").await;
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

    // Finish deployment
    let status = if exit_code == 0 {
        DeployStatus::Success
    } else {
        DeployStatus::Failed
    };
    ctx.finish(status, Some(exit_code), stages).await;

    tracing::info!(
        task_id = %ctx.task_id,
        project = %ctx.project,
        exit_code = exit_code,
        "Docker Compose deployment finished"
    );
}

/// Detect which docker-compose command to use
async fn detect_compose_command() -> (&'static str, Vec<&'static str>) {
    let check = Command::new("which")
        .arg("docker-compose")
        .output()
        .await;

    if check.map(|o| o.status.success()).unwrap_or(false) {
        ("docker-compose", vec![])
    } else {
        ("docker", vec!["compose"])
    }
}
