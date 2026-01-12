//! Docker Compose deployment execution with Blue-Green deployment
//!
//! Handles Docker Compose deployments with zero-downtime:
//! 1. Pull new image
//! 2. Start canary container for health check
//! 3. Wait for Docker HEALTHCHECK to report healthy (smart wait)
//! 4. If healthy, proceed with compose up
//! 5. If unhealthy, abort and keep old container running
//! 6. Post-deploy monitoring for 90 seconds to catch startup issues

use std::time::{Duration, Instant};
use tokio::process::Command;

use crate::domain::deploy::{DeployStage, DeployStatus, StageStatus};

use super::context::DeployContext;

/// Configuration for Docker Compose deployment
pub struct DockerComposeConfig {
    pub compose_file: String,
    pub image: String,
    pub git_repo_dir: Option<String>,
    pub service: Option<String>,
    /// Health check port (default 8081, BFF uses 3001)
    pub health_port: Option<u16>,
}

/// Canary container name prefix (task_id will be appended)
const CANARY_CONTAINER_PREFIX: &str = "deploy-canary";
/// Maximum time to wait for canary to become healthy
const CANARY_MAX_WAIT_SECONDS: u64 = 60;
/// Interval between health status checks
const HEALTH_CHECK_INTERVAL_MS: u64 = 500;
/// Post-deploy monitoring duration
const POST_DEPLOY_MONITOR_SECONDS: u64 = 90;
/// Maximum restarts before triggering alert
const MAX_RESTARTS_BEFORE_ALERT: u32 = 2;

/// Extended fatal error patterns for Rust/Axum applications
/// Note: Patterns are matched case-insensitively
const FATAL_PATTERNS: &[&str] = &[
    // Original patterns
    "panic",
    "fatal error",           // More specific than just "fatal"
    "segmentation fault",
    "core dumped",
    "killed",
    "out of memory",
    "oom",
    // Rust/Axum specific - route conflicts
    "overlapping",           // Route conflict in Axum
    "route conflict",
    "handler already registered",
    "duplicate route",
    // Rust panic patterns
    "thread.*panicked",
    "called `result::unwrap()` on an `err`",
    "called `option::unwrap()` on a `none`",
    "stack backtrace",
    "assertion failed",
    "index out of bounds",
    // System signals
    "sigabrt",
    "sigsegv",
    "sigkill",
    "signal 6",              // SIGABRT
    "signal 9",              // SIGKILL
    "signal 11",             // SIGSEGV
    // Exit codes in logs
    "exit code 134",         // SIGABRT
    "exit code 139",         // SIGSEGV
    "exited with code 1",
    // Database/Network - actual errors only
    "connection refused",
    "connection reset by peer",
    "database.*unavailable",
    "migration failed",
    "cannot start service",
    "no such host",
];

/// Patterns to ignore even if they match FATAL_PATTERNS
/// These are informational messages, not actual errors
const IGNORE_PATTERNS: &[&str] = &[
    "already exists, skipping",      // SQLx migration info
    "if not exists",                 // CREATE IF NOT EXISTS
    "or replace",                    // CREATE OR REPLACE
    "on conflict",                   // UPSERT statements
];

/// Generate a unique canary container name using task_id
/// This prevents conflicts when multiple deployments run concurrently
fn canary_container_name(task_id: &str) -> String {
    // Use first 8 chars of task_id for brevity while maintaining uniqueness
    let short_id = if task_id.len() > 8 {
        &task_id[..8]
    } else {
        task_id
    };
    format!("{}-{}", CANARY_CONTAINER_PREFIX, short_id)
}

/// Clean up any stale canary containers from previous deployments
/// This runs at the start of deployment to ensure a clean state
async fn cleanup_stale_canaries(ctx: &DeployContext) {
    ctx.log_stdout(">>> Cleaning up stale canary containers...")
        .await;

    // Find all containers matching the canary prefix
    let list_result = Command::new("docker")
        .args([
            "ps", "-a", "-q",
            "--filter", &format!("name=^{}", CANARY_CONTAINER_PREFIX),
        ])
        .output()
        .await;

    if let Ok(output) = list_result {
        if output.status.success() {
            let container_ids = String::from_utf8_lossy(&output.stdout);
            let ids: Vec<&str> = container_ids.lines().filter(|s| !s.is_empty()).collect();

            if ids.is_empty() {
                ctx.log_stdout("No stale canary containers found").await;
                return;
            }

            ctx.log_stdout(&format!("Found {} stale canary container(s), removing...", ids.len()))
                .await;

            for id in ids {
                let rm_result = Command::new("docker")
                    .args(["rm", "-f", id.trim()])
                    .output()
                    .await;

                match rm_result {
                    Ok(rm_output) if rm_output.status.success() => {
                        ctx.log_stdout(&format!("  Removed: {}", id.trim())).await;
                    }
                    Ok(rm_output) => {
                        let stderr = String::from_utf8_lossy(&rm_output.stderr);
                        ctx.log_stderr(&format!("  Failed to remove {}: {}", id.trim(), stderr))
                            .await;
                    }
                    Err(e) => {
                        ctx.log_stderr(&format!("  Error removing {}: {}", id.trim(), e))
                            .await;
                    }
                }
            }
        }
    }
}

/// Execute a Docker Compose deployment with blue-green strategy
pub async fn execute(ctx: &DeployContext, work_dir: &str, config: DockerComposeConfig) {
    // Initialize stages - includes canary health check and post-deploy monitoring
    let mut stages = vec![
        DeployStage::new("git_pull", "Git Pull"),
        DeployStage::new("docker_pull", "Docker Pull"),
        DeployStage::new("health_check", "Health Check"),
        DeployStage::new("compose_up", "Compose Up"),
        DeployStage::new("post_deploy_monitor", "Post-Deploy Monitor"),
    ];

    ctx.log_stdout(&format!(
        "=== Docker Compose Deploy (Blue-Green) for {} ===",
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
    ctx.log_stdout("Strategy: Blue-Green (canary health check before swap)")
        .await;

    // Generate unique canary container name for this deployment
    let canary_name = canary_container_name(&ctx.task_id);
    ctx.log_stdout(&format!("Canary container: {}", canary_name))
        .await;

    // Clean up any stale canary containers from previous failed deployments
    cleanup_stale_canaries(ctx).await;

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

    // Stage 3: Canary Health Check (Blue-Green deployment)
    if exit_code == 0 {
        stages[2].start();
        ctx.update_stages(stages.clone()).await;
        ctx.log_stdout("[3/4] Running canary health check...").await;

        // Run canary health check using docker-compose to inherit env vars
        // Default health port is 3000 (monolith), BFF uses 3001
        let health_port = config.health_port.unwrap_or(3000);
        let canary_result = run_canary_health_check(
            ctx,
            &canary_name,
            work_dir,
            &compose_path,
            config.service.as_deref(),
            docker_compose_cmd,
            &docker_compose_args,
            health_port,
        )
        .await;

        match canary_result {
            Ok(()) => {
                stages[2].finish(true, None);
                ctx.log_stdout("✓ Canary health check passed").await;
            }
            Err(e) => {
                stages[2].finish(false, Some(e.clone()));
                ctx.log_stderr(&format!("✗ Canary health check failed: {}", e))
                    .await;
                ctx.log_stderr("Aborting deployment - keeping old container running")
                    .await;
                exit_code = -1;
            }
        }
        ctx.update_stages(stages.clone()).await;
    }

    // Stage 4: Docker compose up -d --force-recreate (only if canary passed)
    if exit_code == 0 {
        stages[3].start();
        ctx.update_stages(stages.clone()).await;
        let service_str = config.service.as_deref().unwrap_or("all services");
        ctx.log_stdout(&format!("[4/4] Swapping to new container: {}...", service_str))
            .await;

        // First, compose pull to ensure compose knows about the new image
        let mut pull_args = docker_compose_args.clone();
        pull_args.extend(["-f", &compose_path, "pull"]);
        if let Some(ref svc) = config.service {
            pull_args.push(svc.as_str());
        }

        ctx.log_stdout(&format!(
            ">>> {} -f {} pull {}",
            docker_compose_cmd,
            compose_path,
            config.service.as_deref().unwrap_or("")
        ))
        .await;

        let _ = Command::new(docker_compose_cmd)
            .args(&pull_args)
            .current_dir(work_dir)
            .output()
            .await;

        // Now do the actual swap with --force-recreate
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
                    ctx.log_stdout("✓ New container is now live").await;
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

    // Stage 5: Post-Deploy Monitoring (only if compose up succeeded)
    if exit_code == 0 {
        stages[4].start();
        ctx.update_stages(stages.clone()).await;
        ctx.log_stdout(&format!(
            "[5/5] Post-deploy monitoring for {} seconds...",
            POST_DEPLOY_MONITOR_SECONDS
        ))
        .await;

        // Get the service/container name to monitor
        let service_name = config.service.as_deref().unwrap_or("xjp-backend");

        // Run post-deploy monitoring
        let monitor_result = run_post_deploy_monitor(
            ctx,
            service_name,
            work_dir,
            &compose_path,
            docker_compose_cmd,
            &docker_compose_args,
        )
        .await;

        match monitor_result {
            Ok(()) => {
                stages[4].finish(true, None);
                ctx.log_stdout("✓ Post-deploy monitoring passed - container is stable").await;
            }
            Err(e) => {
                stages[4].finish(false, Some(e.clone()));
                ctx.log_stderr(&format!("✗ Post-deploy monitoring detected issue: {}", e))
                    .await;
                // Don't fail the deployment, but log a warning
                // The issue has been reported to deploy center
            }
        }
        ctx.update_stages(stages.clone()).await;

        // Show container status
        ctx.log_stdout("").await;
        ctx.log_stdout("=== Deployment Complete (Blue-Green) ===").await;

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

/// Run a canary container to verify the new image works before swapping
///
/// Enhanced with:
/// - Unique canary container name per deployment (prevents conflicts)
/// - Smart wait using Docker HEALTHCHECK status instead of fixed delay
/// - Extended fatal error pattern detection
/// - Strict cleanup with error reporting
async fn run_canary_health_check(
    ctx: &DeployContext,
    canary_name: &str,
    work_dir: &str,
    compose_path: &str,
    service: Option<&str>,
    docker_compose_cmd: &str,
    docker_compose_args: &[&str],
    health_port: u16,
) -> Result<(), String> {
    // Clean up this specific canary container if it exists (from a previous failed attempt)
    ctx.log_stdout(&format!(">>> Ensuring canary container {} is clean...", canary_name))
        .await;
    if let Err(e) = cleanup_canary_strict(ctx, canary_name).await {
        ctx.log_stderr(&format!("Warning: Failed to clean up existing canary: {}", e))
            .await;
        // Continue anyway - the docker run might still succeed
    }

    // Use docker-compose run to start canary with correct environment
    let service_name = service.unwrap_or("xjp-backend");
    let port_mapping = format!("19999:{}", health_port);

    ctx.log_stdout(&format!(
        ">>> {} -f {} run -d --name {} --no-deps -p {} {}",
        docker_compose_cmd, compose_path, canary_name, port_mapping, service_name
    ))
    .await;

    let mut args: Vec<&str> = docker_compose_args.to_vec();
    args.extend([
        "-f", compose_path,
        "run", "-d",
        "--name", canary_name,
        "--no-deps",
        "-p", &port_mapping,
        service_name,
    ]);

    let run_result = Command::new(docker_compose_cmd)
        .args(&args)
        .current_dir(work_dir)
        .output()
        .await
        .map_err(|e| format!("Failed to start canary container: {}", e))?;

    if !run_result.status.success() {
        let stderr = String::from_utf8_lossy(&run_result.stderr);
        let stdout = String::from_utf8_lossy(&run_result.stdout);
        ctx.log_stderr(&format!("Canary start stderr: {}", stderr)).await;
        ctx.log_stdout(&format!("Canary start stdout: {}", stdout)).await;
        let _ = cleanup_canary_strict(ctx, canary_name).await;
        return Err(format!("Failed to start canary container: {}", stderr));
    }

    // Smart wait: poll Docker health status instead of fixed delay
    ctx.log_stdout(&format!(
        ">>> Waiting for canary to become healthy (max {}s)...",
        CANARY_MAX_WAIT_SECONDS
    ))
    .await;

    let wait_result = wait_for_container_healthy(ctx, canary_name, CANARY_MAX_WAIT_SECONDS).await;

    match wait_result {
        Ok(elapsed_secs) => {
            ctx.log_stdout(&format!("✓ Canary became healthy in {:.1}s", elapsed_secs)).await;
        }
        Err(e) => {
            ctx.log_stderr(&format!("✗ Canary health wait failed: {}", e)).await;
            show_container_logs(ctx, canary_name, 50).await;
            let _ = cleanup_canary_strict(ctx, canary_name).await;
            return Err(e);
        }
    }

    // Secondary HTTP health check (in case Docker HEALTHCHECK isn't configured)
    ctx.log_stdout(">>> curl -sf http://127.0.0.1:19999/health").await;

    let health_check = Command::new("curl")
        .args(["-sf", "--max-time", "5", "http://127.0.0.1:19999/health"])
        .output()
        .await;

    match health_check {
        Ok(output) if output.status.success() => {
            ctx.log_stdout("✓ Canary HTTP health check passed").await;
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            ctx.log_stderr(&format!("✗ Canary HTTP check failed: {}", stderr)).await;
            show_container_logs(ctx, canary_name, 30).await;
            let _ = cleanup_canary_strict(ctx, canary_name).await;
            return Err("Canary HTTP health check failed".to_string());
        }
        Err(e) => {
            ctx.log_stderr(&format!("✗ Failed to run HTTP check: {}", e)).await;
            let _ = cleanup_canary_strict(ctx, canary_name).await;
            return Err(format!("Failed to run health check: {}", e));
        }
    }

    // Check container logs for fatal error patterns
    ctx.log_stdout(">>> Checking canary logs for fatal patterns...").await;
    if let Some(pattern) = check_logs_for_fatal_patterns(canary_name).await {
        ctx.log_stderr(&format!("✗ Found fatal pattern in logs: '{}'", pattern)).await;
        show_container_logs(ctx, canary_name, 30).await;
        let _ = cleanup_canary_strict(ctx, canary_name).await;
        return Err(format!("Canary container has fatal error: {}", pattern));
    }
    ctx.log_stdout("✓ No fatal patterns found in logs").await;

    // Container is healthy - clean up canary
    ctx.log_stdout(&format!(">>> Cleaning up canary container {}...", canary_name)).await;
    if let Err(e) = cleanup_canary_strict(ctx, canary_name).await {
        // Log but don't fail - the deployment itself succeeded
        ctx.log_stderr(&format!("Warning: Failed to clean up canary: {}", e)).await;
    }

    Ok(())
}

/// Wait for a container to become healthy using Docker's HEALTHCHECK status
///
/// Returns the elapsed time in seconds if healthy, or an error message
async fn wait_for_container_healthy(
    ctx: &DeployContext,
    container: &str,
    max_wait_secs: u64,
) -> Result<f64, String> {
    let start = Instant::now();
    let max_duration = Duration::from_secs(max_wait_secs);
    let check_interval = Duration::from_millis(HEALTH_CHECK_INTERVAL_MS);

    loop {
        if start.elapsed() > max_duration {
            return Err(format!("Timeout after {}s waiting for healthy status", max_wait_secs));
        }

        // Get container state
        let inspect_result = Command::new("docker")
            .args([
                "inspect",
                "--format",
                "{{.State.Status}}|{{.State.Health.Status}}|{{.State.ExitCode}}",
                container,
            ])
            .output()
            .await;

        match inspect_result {
            Ok(output) if output.status.success() => {
                let info = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let parts: Vec<&str> = info.split('|').collect();

                let container_status = parts.first().unwrap_or(&"unknown");
                let health_status = parts.get(1).unwrap_or(&"none");
                let exit_code = parts.get(2).unwrap_or(&"0");

                // Check for container death
                if *container_status == "exited" || *container_status == "dead" {
                    return Err(format!(
                        "Container died (status: {}, exit_code: {})",
                        container_status, exit_code
                    ));
                }

                // Check Docker HEALTHCHECK status
                match *health_status {
                    "healthy" => {
                        return Ok(start.elapsed().as_secs_f64());
                    }
                    "unhealthy" => {
                        // Get health check logs
                        let health_logs = get_health_check_logs(container).await;
                        return Err(format!("Container marked unhealthy: {}", health_logs));
                    }
                    "starting" | "none" | "" => {
                        // Still starting or no healthcheck defined
                        // For containers without HEALTHCHECK, fall back to "running" status
                        if *health_status == "none" || *health_status == "" {
                            if *container_status == "running" {
                                // Give it a moment to stabilize, then accept
                                if start.elapsed().as_secs() >= 3 {
                                    ctx.log_stdout("Note: No Docker HEALTHCHECK defined, using running status").await;
                                    return Ok(start.elapsed().as_secs_f64());
                                }
                            }
                        }
                        // Continue waiting
                    }
                    other => {
                        ctx.log_stdout(&format!("Unknown health status: {}", other)).await;
                    }
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stderr.contains("No such object") {
                    return Err("Container no longer exists".to_string());
                }
            }
            Err(e) => {
                return Err(format!("Failed to inspect container: {}", e));
            }
        }

        tokio::time::sleep(check_interval).await;
    }
}

/// Get the last health check log from Docker
async fn get_health_check_logs(container: &str) -> String {
    let result = Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{range .State.Health.Log}}{{.Output}}{{end}}",
            container,
        ])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            let logs = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if logs.is_empty() {
                "No health check logs available".to_string()
            } else {
                // Get last 200 chars
                if logs.len() > 200 {
                    format!("...{}", &logs[logs.len() - 200..])
                } else {
                    logs
                }
            }
        }
        _ => "Failed to get health check logs".to_string(),
    }
}

/// Check container logs for fatal error patterns
async fn check_logs_for_fatal_patterns(container: &str) -> Option<String> {
    let logs_result = Command::new("docker")
        .args(["logs", "--tail", "100", container])
        .output()
        .await;

    if let Ok(logs) = logs_result {
        let stdout = String::from_utf8_lossy(&logs.stdout);
        let stderr = String::from_utf8_lossy(&logs.stderr);

        // Check each line individually to properly handle ignore patterns
        let all_lines: Vec<&str> = stdout.lines().chain(stderr.lines()).collect();

        for line in all_lines {
            let line_lower = line.to_lowercase();

            // Skip lines that match ignore patterns (informational messages)
            let should_ignore = IGNORE_PATTERNS.iter().any(|p| line_lower.contains(p));
            if should_ignore {
                continue;
            }

            // Check for fatal patterns
            for pattern in FATAL_PATTERNS {
                if line_lower.contains(pattern) {
                    return Some(format!("{} (in: {})", pattern,
                        if line.len() > 80 { &line[..80] } else { line }));
                }
            }
        }
    }

    None
}

/// Show container logs
async fn show_container_logs(ctx: &DeployContext, container: &str, lines: u32) {
    ctx.log_stdout(&format!(">>> docker logs {} (last {} lines)", container, lines)).await;
    if let Ok(logs) = Command::new("docker")
        .args(["logs", "--tail", &lines.to_string(), container])
        .output()
        .await
    {
        let stdout = String::from_utf8_lossy(&logs.stdout);
        let stderr = String::from_utf8_lossy(&logs.stderr);
        if !stdout.is_empty() {
            ctx.log_stdout(&format!("[stdout]\n{}", stdout)).await;
        }
        if !stderr.is_empty() {
            ctx.log_stderr(&format!("[stderr]\n{}", stderr)).await;
        }
    }
}

/// Clean up a specific canary container with strict error checking
///
/// Unlike the old best-effort cleanup, this function:
/// 1. Checks if the container exists first
/// 2. Reports errors instead of silently ignoring them
/// 3. Logs cleanup actions for debugging
async fn cleanup_canary_strict(ctx: &DeployContext, container_name: &str) -> Result<(), String> {
    // Check if container exists
    let check_result = Command::new("docker")
        .args(["ps", "-a", "-q", "-f", &format!("name=^{}$", container_name)])
        .output()
        .await
        .map_err(|e| format!("Failed to check if container exists: {}", e))?;

    if !check_result.status.success() {
        return Err(format!(
            "Failed to check container: {}",
            String::from_utf8_lossy(&check_result.stderr)
        ));
    }

    let container_id = String::from_utf8_lossy(&check_result.stdout).trim().to_string();
    if container_id.is_empty() {
        // Container doesn't exist, nothing to clean up
        return Ok(());
    }

    // Container exists, remove it
    ctx.log_stdout(&format!(">>> docker rm -f {}", container_name)).await;

    let rm_result = Command::new("docker")
        .args(["rm", "-f", container_name])
        .output()
        .await
        .map_err(|e| format!("Failed to execute docker rm: {}", e))?;

    if rm_result.status.success() {
        ctx.log_stdout(&format!("✓ Removed canary container: {}", container_name)).await;
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&rm_result.stderr);
        Err(format!("Failed to remove container {}: {}", container_name, stderr))
    }
}

/// Post-deploy monitoring: watch the production container for issues after deployment
///
/// Monitors for:
/// - Container restarts (crash loops)
/// - Container death
/// - Health check failures
/// - Fatal error patterns in logs
async fn run_post_deploy_monitor(
    ctx: &DeployContext,
    service_name: &str,
    work_dir: &str,
    compose_path: &str,
    docker_compose_cmd: &str,
    docker_compose_args: &[&str],
) -> Result<(), String> {
    let start = Instant::now();
    let duration = Duration::from_secs(POST_DEPLOY_MONITOR_SECONDS);
    let check_interval = Duration::from_secs(5);

    // Get the actual container name from docker-compose
    let container_name = get_compose_container_name(
        work_dir,
        compose_path,
        service_name,
        docker_compose_cmd,
        docker_compose_args,
    )
    .await
    .unwrap_or_else(|| service_name.to_string());

    ctx.log_stdout(&format!("Monitoring container: {}", container_name)).await;

    let mut initial_restart_count: Option<u32> = None;
    let mut last_log_position = 0u64;

    while start.elapsed() < duration {
        // Check container state
        let state = get_container_state(&container_name).await;

        match state {
            ContainerState::Running { restart_count, health_status } => {
                // Track restart count
                if initial_restart_count.is_none() {
                    initial_restart_count = Some(restart_count);
                }

                let restarts_since_deploy = restart_count - initial_restart_count.unwrap_or(0);

                if restarts_since_deploy > 0 {
                    ctx.log_stderr(&format!(
                        "⚠ Container has restarted {} time(s) since deployment!",
                        restarts_since_deploy
                    )).await;

                    // Report to deploy center
                    report_post_deploy_issue(
                        ctx,
                        &format!("Container restarted {} times", restarts_since_deploy),
                        restart_count,
                    ).await;

                    if restarts_since_deploy >= MAX_RESTARTS_BEFORE_ALERT {
                        return Err(format!(
                            "Container in crash loop: {} restarts since deployment",
                            restarts_since_deploy
                        ));
                    }
                }

                // Check health status
                if health_status == "unhealthy" {
                    ctx.log_stderr("⚠ Container health check is failing!").await;
                    report_post_deploy_issue(ctx, "Container unhealthy", restart_count).await;
                }

                // Check for new fatal errors in logs
                if let Some(pattern) = check_new_logs_for_fatal_patterns(
                    &container_name,
                    &mut last_log_position,
                ).await {
                    ctx.log_stderr(&format!("⚠ Fatal pattern detected in logs: {}", pattern)).await;
                    report_post_deploy_issue(ctx, &format!("Fatal error: {}", pattern), restart_count).await;
                }
            }
            ContainerState::Exited { exit_code } => {
                ctx.log_stderr(&format!("✗ Container exited with code {}", exit_code)).await;
                show_container_logs(ctx, &container_name, 50).await;
                report_post_deploy_issue(ctx, &format!("Container exited (code {})", exit_code), 0).await;
                return Err(format!("Container exited with code {}", exit_code));
            }
            ContainerState::NotFound => {
                ctx.log_stderr("✗ Container not found!").await;
                return Err("Container no longer exists".to_string());
            }
            ContainerState::Unknown(status) => {
                ctx.log_stderr(&format!("⚠ Unknown container status: {}", status)).await;
            }
        }

        // Progress indicator every 30 seconds
        let elapsed = start.elapsed().as_secs();
        if elapsed % 30 == 0 && elapsed > 0 {
            ctx.log_stdout(&format!(
                "... monitoring: {}s / {}s",
                elapsed, POST_DEPLOY_MONITOR_SECONDS
            )).await;
        }

        tokio::time::sleep(check_interval).await;
    }

    Ok(())
}

/// Container state for monitoring
enum ContainerState {
    Running { restart_count: u32, health_status: String },
    Exited { exit_code: i32 },
    NotFound,
    Unknown(String),
}

/// Get container state including restart count
async fn get_container_state(container: &str) -> ContainerState {
    let result = Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{.State.Status}}|{{.State.Health.Status}}|{{.RestartCount}}|{{.State.ExitCode}}",
            container,
        ])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            let info = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let parts: Vec<&str> = info.split('|').collect();

            let status = parts.first().unwrap_or(&"unknown");
            let health = parts.get(1).unwrap_or(&"none").to_string();
            let restart_count: u32 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            let exit_code: i32 = parts.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);

            match *status {
                "running" => ContainerState::Running { restart_count, health_status: health },
                "exited" | "dead" => ContainerState::Exited { exit_code },
                other => ContainerState::Unknown(other.to_string()),
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No such object") {
                ContainerState::NotFound
            } else {
                ContainerState::Unknown(stderr.to_string())
            }
        }
        Err(_) => ContainerState::NotFound,
    }
}

/// Get the actual container name from docker-compose
async fn get_compose_container_name(
    work_dir: &str,
    compose_path: &str,
    service_name: &str,
    docker_compose_cmd: &str,
    docker_compose_args: &[&str],
) -> Option<String> {
    let mut args: Vec<&str> = docker_compose_args.to_vec();
    args.extend(["-f", compose_path, "ps", "-q", service_name]);

    let result = Command::new(docker_compose_cmd)
        .args(&args)
        .current_dir(work_dir)
        .output()
        .await
        .ok()?;

    if result.status.success() {
        let container_id = String::from_utf8_lossy(&result.stdout).trim().to_string();
        if !container_id.is_empty() {
            // Get container name from ID
            let name_result = Command::new("docker")
                .args(["inspect", "--format", "{{.Name}}", &container_id])
                .output()
                .await
                .ok()?;

            if name_result.status.success() {
                let name = String::from_utf8_lossy(&name_result.stdout)
                    .trim()
                    .trim_start_matches('/')
                    .to_string();
                return Some(name);
            }
        }
    }

    None
}

/// Check for new fatal patterns in container logs since last check
async fn check_new_logs_for_fatal_patterns(
    container: &str,
    last_position: &mut u64,
) -> Option<String> {
    // Get logs since a timestamp (simplified: just get recent logs)
    let logs_result = Command::new("docker")
        .args(["logs", "--tail", "20", container])
        .output()
        .await;

    if let Ok(logs) = logs_result {
        let stdout = String::from_utf8_lossy(&logs.stdout).to_lowercase();
        let stderr = String::from_utf8_lossy(&logs.stderr).to_lowercase();
        let combined = format!("{}{}", stdout, stderr);

        // Simple dedup: track combined length
        let new_length = combined.len() as u64;
        if new_length > *last_position {
            *last_position = new_length;

            for pattern in FATAL_PATTERNS {
                if combined.contains(pattern) {
                    return Some(pattern.to_string());
                }
            }
        }
    }

    None
}

/// Report a post-deploy issue to the deploy center
async fn report_post_deploy_issue(
    ctx: &DeployContext,
    issue: &str,
    restart_count: u32,
) {
    // Log the issue
    ctx.log_stderr(&format!("[POST-DEPLOY ISSUE] {}", issue)).await;

    // Report to deploy center via existing log append mechanism
    ctx.state
        .deploy_center
        .append_log_simple(
            &ctx.task_id,
            "stderr",
            &format!("[POST-DEPLOY ALERT] {} (restarts: {})", issue, restart_count),
        )
        .await;

    // TODO: In the future, add a dedicated endpoint for post-deploy alerts
    // that can trigger notifications, auto-rollback, etc.
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
