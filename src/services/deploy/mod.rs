//! 部署服务模块
//!
//! 包含所有部署类型的执行逻辑

pub mod context;
pub mod docker_build;
pub mod docker_compose;
pub mod script;
pub mod xiaojincut;

use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::api::deploy::TriggerRequest;
use crate::config::env::constants::{DEPLOY_TIMEOUT_SECS, HEARTBEAT_INTERVAL_SECS};
use crate::config::project::ProjectConfig;
use crate::domain::deploy::{DeployStatus, DeployType};
use crate::state::AppState;

pub use context::DeployContext;
pub use docker_build::DockerBuildConfig;
pub use docker_compose::DockerComposeConfig;
pub use xiaojincut::XiaojincutConfig;

/// 执行部署任务
///
/// 这是部署的主入口点，根据项目配置选择对应的部署策略
pub async fn execute(
    state: Arc<AppState>,
    task_id: String,
    project: String,
    project_config: ProjectConfig,
    request: TriggerRequest,
) {
    // 执行单个部署
    execute_single(
        state.clone(),
        task_id,
        project.clone(),
        project_config,
        request,
    )
    .await;

    // 处理队列中的后续部署
    process_queue(state, project).await;
}

/// 执行单个部署任务（不处理队列）
async fn execute_single(
    state: Arc<AppState>,
    task_id: String,
    project: String,
    project_config: ProjectConfig,
    request: TriggerRequest,
) {
    // 获取日志通道
    let log_tx = state.log_hub.get_sender(&task_id).await;
    let cancel_token = state
        .running_deploys
        .read()
        .await
        .get(&project)
        .map(|d| d.cancel_token.clone())
        .unwrap_or_else(CancellationToken::new);

    // 创建部署上下文
    let ctx = DeployContext {
        task_id: task_id.clone(),
        project: project.clone(),
        state: state.clone(),
        log_tx,
        cancel_token: cancel_token.clone(),
        commit_hash: request.commit_hash,
        branch: request.branch,
    };

    // 启动心跳任务
    let heartbeat_task = spawn_heartbeat(state.clone(), task_id.clone(), cancel_token.clone());

    // 启动超时保护
    let timeout_task = spawn_timeout(task_id.clone(), cancel_token.clone());

    // 根据项目类型执行对应的部署逻辑
    match project_config.deploy_type {
        DeployType::Script { script } => {
            script::execute(&ctx, &project_config.work_dir, &script).await;
        }
        DeployType::DockerCompose {
            compose_file,
            image,
            git_repo_dir,
            service,
        } => {
            let config = DockerComposeConfig {
                compose_file,
                image,
                git_repo_dir,
                service,
            };
            docker_compose::execute(&ctx, &project_config.work_dir, config).await;
        }
        DeployType::DockerBuild {
            dockerfile,
            build_context,
            image,
            tag,
            push_latest,
            branch,
        } => {
            let config = DockerBuildConfig {
                dockerfile,
                build_context,
                image,
                tag,
                push_latest,
                branch,
            };
            docker_build::execute(&ctx, &project_config.work_dir, config).await;
        }
        DeployType::Xiaojincut {
            task_type,
            input_path,
            output_dir,
            prompt,
            port,
        } => {
            let config = XiaojincutConfig {
                task_type,
                input_path,
                output_dir,
                prompt,
                port,
            };
            xiaojincut::execute(&ctx, config).await;
        }
    }

    // 取消辅助任务
    heartbeat_task.abort();
    timeout_task.abort();

    // 取消注册当前部署
    state.unregister_running_deploy(&project).await;
}

/// 处理队列中的后续部署
async fn process_queue(state: Arc<AppState>, project: String) {
    // 循环处理队列中的所有部署
    loop {
        // 从队列中取出下一个部署
        let next = state.dequeue_deploy(&project).await;
        let Some(next) = next else {
            break;
        };

        tracing::info!(
            task_id = %next.task_id,
            project = %project,
            "Starting queued deployment"
        );

        // 更新任务状态为 running
        state
            .task_store
            .update_status(&next.task_id, DeployStatus::Running, None)
            .await;

        // 注册新的运行中部署
        let _cancel_token = state.register_running_deploy(&project, &next.task_id).await;

        // 创建日志通道
        state.log_hub.create(&next.task_id).await;

        // 执行部署
        execute_single(
            state.clone(),
            next.task_id,
            project.clone(),
            next.project_config,
            next.request,
        )
        .await;
    }
}

/// 启动心跳任务
fn spawn_heartbeat(
    state: Arc<AppState>,
    task_id: String,
    cancel_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => break,
                _ = interval.tick() => {
                    state.deploy_center.heartbeat(&task_id).await;
                }
            }
        }
    })
}

/// 启动超时保护任务
fn spawn_timeout(task_id: String, cancel_token: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(DEPLOY_TIMEOUT_SECS)).await;
        tracing::error!(task_id = %task_id, "Deployment timed out after {} minutes", DEPLOY_TIMEOUT_SECS / 60);
        cancel_token.cancel();
    })
}
