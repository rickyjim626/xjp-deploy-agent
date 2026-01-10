# Task Recovery System Design

## Problem Statement

当 Deploy Agent 重启时（手动重启、自动更新、崩溃），所有内存中的任务会丢失：
- 正在执行的任务（running）
- 队列中等待的任务（queued）

这导致：
1. Deploy Center 显示任务卡在 "running" 状态
2. 用户需要手动重新触发部署
3. 自动更新会导致当前构建任务丢失

## Current Architecture Analysis

### Deploy Center (PostgreSQL)
```
deploy_logs:
  - id: UUID
  - status: pending | running | success | failed
  - executor_name: "privatecloud-agent"
  - last_heartbeat_at: timestamp
  - stages: JSONB
```

已有机制：
- ✅ Heartbeat 心跳检测
- ✅ `reconcile_stale_logs` API（根据日志内容判断状态）
- ✅ `mark_old_logs` API（批量标记超时任务）

### Deploy Agent (In-Memory)
```rust
pub struct AppState {
    pub task_store: TaskStore,           // HashMap<task_id, DeployTask>
    pub deploy_queue: RwLock<HashMap>,   // project -> VecDeque<QueuedDeploy>
    pub running_deploys: RwLock<HashMap>, // project -> RunningDeploy
}
```

缺失机制：
- ❌ 任务持久化
- ❌ 启动时恢复
- ❌ 优雅关闭（等待任务完成）

## Solution Design

### Phase 1: Task Persistence (Agent 端)

**目标**: 任务入队时持久化到本地文件，重启后可恢复

#### 1.1 Queue Persistence Module

```rust
// src/services/queue_persistence.rs

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;

const QUEUE_FILE: &str = "/opt/xjp-deploy-agent/queue.json";

#[derive(Serialize, Deserialize)]
pub struct PersistedQueue {
    pub version: u32,
    pub tasks: Vec<PersistedTask>,
    pub saved_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Deserialize)]
pub struct PersistedTask {
    pub task_id: String,
    pub project: String,
    pub deploy_log_id: Option<String>,
    pub commit_hash: Option<String>,
    pub branch: Option<String>,
    pub status: String,  // "queued" | "running"
    pub queued_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl PersistedQueue {
    /// Load queue from disk
    pub async fn load() -> Option<Self> {
        let content = fs::read_to_string(QUEUE_FILE).await.ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save queue to disk (atomic write)
    pub async fn save(&self) -> anyhow::Result<()> {
        let temp_file = format!("{}.tmp", QUEUE_FILE);
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&temp_file, &content).await?;
        fs::rename(&temp_file, QUEUE_FILE).await?;
        Ok(())
    }

    /// Clear queue file
    pub async fn clear() -> anyhow::Result<()> {
        if PathBuf::from(QUEUE_FILE).exists() {
            fs::remove_file(QUEUE_FILE).await?;
        }
        Ok(())
    }
}
```

#### 1.2 Integration Points

**入队时持久化** (`src/api/deploy.rs`):
```rust
// After enqueue_deploy()
let persisted = PersistedQueue {
    version: 1,
    tasks: collect_all_tasks(&state).await,
    saved_at: Utc::now(),
};
persisted.save().await?;
```

**任务完成时更新** (`src/services/deploy/mod.rs`):
```rust
// After task completion
remove_from_persisted_queue(&task_id).await;
```

**启动时恢复** (`src/lib.rs`):
```rust
// In run_agent_with_config(), after AppState::new()
if let Some(queue) = PersistedQueue::load().await {
    recover_tasks(&state, queue).await;
}
```

### Phase 2: Graceful Shutdown for Auto-Update

**目标**: 自动更新时，等待当前任务完成再重启

#### 2.1 Update Strategy Changes

```rust
// src/services/autoupdate.rs

async fn perform_update(state: Arc<AppState>, ...) {
    // 1. Check if any task is running
    let running = state.running_deploys.read().await;
    if !running.is_empty() {
        tracing::info!("Deferring update: {} tasks running", running.len());

        // Mark update as pending
        state.auto_update_state.set_pending_update(Some(metadata.clone()));

        // Don't proceed with update
        return;
    }

    // 2. Set update lock to prevent new tasks
    state.auto_update_state.set_update_in_progress(true);

    // 3. Wait for queue to drain (with timeout)
    let drain_timeout = Duration::from_secs(300); // 5 min
    let start = Instant::now();
    while !state.deploy_queue.read().await.is_empty() {
        if start.elapsed() > drain_timeout {
            tracing::warn!("Queue drain timeout, proceeding with update");
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // 4. Proceed with update
    do_update_restart(...).await;
}
```

#### 2.2 Task Completion Hook

```rust
// src/services/deploy/mod.rs

async fn execute_single(...) {
    // ... existing execution logic ...

    // After task completes, check for pending update
    if state.auto_update_state.has_pending_update() {
        let running = state.running_deploys.read().await;
        if running.is_empty() {
            tracing::info!("All tasks complete, triggering pending update");
            // Trigger the deferred update
            tokio::spawn(async move {
                services::autoupdate::trigger_pending_update(state).await;
            });
        }
    }
}
```

### Phase 3: Deploy Center Recovery Query

**目标**: Agent 启动时查询 Deploy Center 是否有未完成的任务

#### 3.1 New Deploy Center API

```rust
// services/deploy-center/src/api/agents.rs

/// GET /api/deploy/agents/:agent_name/pending-tasks
///
/// Returns tasks assigned to this agent that are still pending/running
pub async fn get_pending_tasks(
    State(state): State<AppState>,
    Path(agent_name): Path<String>,
) -> Result<Json<Vec<PendingTask>>, ApiError> {
    let tasks = state.logs_repo
        .get_pending_for_executor(&agent_name)
        .await?;

    Ok(Json(tasks))
}

#[derive(Serialize)]
pub struct PendingTask {
    pub deploy_log_id: Uuid,
    pub project: String,
    pub commit_hash: Option<String>,
    pub branch: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
}
```

#### 3.2 Agent Startup Recovery

```rust
// src/lib.rs

async fn recover_from_deploy_center(state: &Arc<AppState>) {
    let Some(callback_url) = &state.config.callback_url else {
        return;
    };

    let agent_name = &state.config.agent_name;
    let url = format!("{}/api/deploy/agents/{}/pending-tasks", callback_url, agent_name);

    match reqwest::get(&url).await {
        Ok(resp) if resp.status().is_success() => {
            let tasks: Vec<PendingTask> = resp.json().await.unwrap_or_default();

            for task in tasks {
                // Check if task is stale (no heartbeat > 5 min)
                if is_stale(&task) {
                    // Mark as failed in Deploy Center
                    notify_task_failed(&task, "Agent restarted, task abandoned").await;
                } else {
                    // Re-queue for execution
                    re_queue_task(&state, task).await;
                }
            }
        }
        _ => {
            tracing::warn!("Failed to query Deploy Center for pending tasks");
        }
    }
}
```

### Phase 4: Heartbeat-Based Auto-Recovery

**目标**: Deploy Center 检测到心跳超时后自动恢复

#### 4.1 Enhanced Deploy Center Monitor

```rust
// services/deploy-center/src/services/no_response_monitor.rs

/// Background task that monitors for stale deployments
pub async fn start_no_response_monitor(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));

    loop {
        interval.tick().await;

        // Find logs with no heartbeat for > 2 minutes
        let stale_logs = state.logs_repo
            .find_stale_running_logs(Duration::from_secs(120))
            .await;

        for log in stale_logs {
            tracing::warn!(
                id = %log.id,
                project = %log.project,
                last_heartbeat = ?log.last_heartbeat_at,
                "Detected stale deployment, marking as failed"
            );

            // Mark as failed with reason
            state.logs_repo.update(log.id, &UpdateDeployLogRequest {
                status: Some("failed".to_string()),
                error_message: Some("Agent heartbeat timeout - task may need retry".to_string()),
                ..Default::default()
            }).await;

            // Broadcast status change for UI
            state.broadcast_event(DeployEvent::DeployStatusChanged {
                deploy_log_id: log.id.to_string(),
                status: "failed".to_string(),
                ..
            });
        }
    }
}
```

## Implementation Phases

| Phase | Effort | Impact | Priority |
|-------|--------|--------|----------|
| 1. Task Persistence | Medium | High | P0 |
| 2. Graceful Auto-Update | Low | High | P0 |
| 3. Deploy Center Query | Medium | Medium | P1 |
| 4. Heartbeat Monitor | Low | Medium | P1 |

## Recommended Implementation Order

1. **Phase 2 (Graceful Auto-Update)** - 最简单，立即解决自更新导致的任务丢失
2. **Phase 1 (Task Persistence)** - 解决所有重启场景
3. **Phase 4 (Heartbeat Monitor)** - Deploy Center 端自动处理超时
4. **Phase 3 (Recovery Query)** - 双向同步，最完整但复杂度最高

## File Changes Summary

### Phase 1 & 2 (Agent)
- NEW: `src/services/queue_persistence.rs`
- MODIFY: `src/services/mod.rs`
- MODIFY: `src/api/deploy.rs` (persist on enqueue)
- MODIFY: `src/services/deploy/mod.rs` (update on complete, check pending update)
- MODIFY: `src/services/autoupdate.rs` (defer update if tasks running)
- MODIFY: `src/lib.rs` (recover on startup)
- MODIFY: `src/config/autoupdate.rs` (add pending_update field)

### Phase 3 & 4 (Deploy Center)
- NEW: `services/deploy-center/src/api/agent_recovery.rs`
- NEW: `services/deploy-center/src/services/no_response_monitor.rs`
- MODIFY: `services/deploy-center/src/db/logs.rs` (add find_stale_running_logs)
- MODIFY: `services/deploy-center/src/main.rs` (start monitor task)

## Testing Strategy

1. **Unit Tests**
   - PersistedQueue serialization/deserialization
   - Atomic file write/read

2. **Integration Tests**
   - Simulate restart during task execution
   - Verify task recovery on startup
   - Verify graceful update waits for completion

3. **Manual Testing**
   - Trigger xjp-deploy-agent build
   - Kill agent mid-build
   - Verify task is recovered or properly marked failed
