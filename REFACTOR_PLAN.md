# xjp-deploy-agent 重构方案

## 现状分析确认

基于代码调查，确认以下问题：

| 问题类型 | 具体位置 | 影响 |
|---------|---------|------|
| 单文件过大 | `main.rs` 6052行 | 难以导航、维护、测试 |
| 类型定义散乱 | 60+ struct/enum 混在一起 | 职责不清 |
| 鉴权重复 | 每个 handler 手动 `state.verify_api_key(&headers)` | 代码重复、易漏 |
| log_channels 泄漏 | 只创建不删除，SSE 结束无通知 | 内存持续增长 |
| 任务无清理策略 | `tasks` 只增不减 | 内存持续增长 |
| HTTP client 无复用 | 每次请求 `reqwest::get/post` | 连接开销大 |
| 配置解析散乱 | `AppState::new()` 176行 | 难以测试、扩展 |

## 目标架构

重构后 main.rs 应只包含：
```rust
// ~100 行
fn main() {
    // 1. tracing 初始化
    // 2. 加载配置
    // 3. 构建 Router
    // 4. 启动后台任务
    // 5. 启动 HTTP server
}
```

## 推荐目录结构

```
src/
├── main.rs                 # 入口 (~100行)
├── lib.rs                  # 模块导出
│
├── domain/                 # 纯数据结构 (无 axum/tokio 依赖)
│   ├── mod.rs
│   ├── deploy.rs           # DeployTask, DeployStage, DeployStatus, LogLine
│   ├── system.rs           # SystemInfo, SystemStats, DiskInfo
│   ├── container.rs        # ContainerInfo, EnvVar
│   ├── database.rs         # DatabaseType, DbConnectionConfig, TableInfo...
│   └── tunnel.rs           # TunnelMode, PortMapping, TunnelMessage
│
├── config/                 # 配置加载
│   ├── mod.rs
│   ├── env.rs              # 环境变量解析
│   └── project.rs          # ProjectConfig, RemoteAgentConfig
│
├── state/                  # 运行时状态
│   ├── mod.rs
│   ├── app_state.rs        # AppState 定义
│   ├── task_store.rs       # TaskStore (tasks + history + 清理策略)
│   └── log_hub.rs          # LogHub (create/subscribe/finish/cleanup)
│
├── services/               # 业务逻辑
│   ├── mod.rs
│   ├── deploy/
│   │   ├── mod.rs
│   │   ├── context.rs      # DeployContext 统一上下文
│   │   ├── script.rs       # 脚本部署
│   │   ├── docker_compose.rs
│   │   ├── docker_build.rs
│   │   └── xiaojincut.rs
│   ├── database/
│   │   ├── mod.rs
│   │   ├── postgres.rs
│   │   └── mysql.rs
│   ├── tunnel/
│   │   ├── mod.rs
│   │   ├── server.rs
│   │   └── client.rs
│   └── autoupdate.rs
│
├── api/                    # HTTP handlers
│   ├── mod.rs              # Router 组装
│   ├── health.rs           # /health, /status
│   ├── deploy.rs           # /trigger/:project, /tasks/*, /logs/*
│   ├── system.rs           # /system/*
│   ├── containers.rs       # /containers/*
│   ├── database.rs         # /db/*
│   └── tunnel.rs           # /tunnel/*
│
├── infra/                  # 外部依赖封装
│   ├── mod.rs
│   ├── deploy_center.rs    # DeployCenterClient (callback/heartbeat/log)
│   └── command.rs          # CommandRunner (spawn/stream/timeout/cancel)
│
├── middleware/             # Axum 中间件
│   ├── mod.rs
│   └── auth.rs             # ApiKeyAuth extractor/middleware
│
└── error.rs                # 统一 ApiError -> IntoResponse
```

## 迁移步骤（小步提交）

### Phase 1: 基础设施 (4 commits)

#### 1.1 创建 error.rs - 统一错误处理
```rust
// src/error.rs
use axum::{http::StatusCode, response::{IntoResponse, Response}, Json};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

pub enum ApiError {
    Unauthorized,
    NotFound(String),
    BadRequest(String),
    Internal(String),
    Conflict(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error, details) = match self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized", None),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, "Not found", Some(msg)),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "Bad request", Some(msg)),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, "Internal error", Some(msg)),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, "Conflict", Some(msg)),
        };
        (status, Json(ErrorResponse { error: error.to_string(), details })).into_response()
    }
}
```

#### 1.2 创建 middleware/auth.rs - 统一鉴权
```rust
// src/middleware/auth.rs
use axum::{extract::FromRequestParts, http::request::Parts};
use crate::{state::AppState, error::ApiError};

pub struct RequireApiKey;

#[async_trait::async_trait]
impl FromRequestParts<Arc<AppState>> for RequireApiKey {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let key = parts.headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok());

        match key {
            Some(k) if k == state.api_key => Ok(RequireApiKey),
            _ => Err(ApiError::Unauthorized),
        }
    }
}
```

#### 1.3 创建 infra/deploy_center.rs - HTTP client 复用
```rust
// src/infra/deploy_center.rs
use reqwest::Client;
use std::time::Duration;

pub struct DeployCenterClient {
    client: Client,
    callback_url: Option<String>,
}

impl DeployCenterClient {
    pub fn new(callback_url: Option<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(5)
            .build()
            .expect("Failed to create HTTP client");
        Self { client, callback_url }
    }

    pub async fn notify_status(&self, task_id: &str, status: &str, stages: &[DeployStage]) -> Result<(), reqwest::Error>;
    pub async fn append_log(&self, task_id: &str, line: &LogLine) -> Result<(), reqwest::Error>;
    pub async fn heartbeat(&self, task_id: &str) -> Result<(), reqwest::Error>;
    pub async fn fetch_config(&self, project: &str) -> Result<RemoteAgentConfig, reqwest::Error>;
}
```

#### 1.4 创建 infra/command.rs - 统一命令执行
```rust
// src/infra/command.rs
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub struct CommandRunner;

impl CommandRunner {
    pub async fn run_with_streaming(
        cmd: &str,
        args: &[&str],
        work_dir: &Path,
        log_sink: broadcast::Sender<LogLine>,
        cancel: CancellationToken,
        timeout: Duration,
    ) -> Result<ExitStatus, CommandError>;
}
```

### Phase 2: Domain 层抽取 (5 commits)

#### 2.1 domain/deploy.rs
从 main.rs 迁移：
- `DeployStatus` (45-49)
- `StageStatus` (54-60)
- `DeployStage` (64-113)
- `DeployTask` (117-127)
- `LogLine` (131-135)
- `DeployType` (139-181)

#### 2.2 domain/system.rs
- `SystemInfo` (303-313)
- `DiskInfo` (317-324)
- `SystemStats` (328-339)
- `LoadAverage` (343-347)

#### 2.3 domain/container.rs
- `ContainerInfo` (374-382)
- `EnvVar` (417-423)
- 相关 Request/Response 结构

#### 2.4 domain/database.rs
- `DatabaseType` (437-440)
- `DbConnectionConfig` (453-466)
- 所有 Db* 结构体

#### 2.5 domain/tunnel.rs
- `TunnelMode` (660-667)
- `PortMapping` (681-714)
- `TunnelMessage` (719-735)
- `TunnelConnectionInfo`, `PortMappingStatus`, etc.

### Phase 3: Config 层抽取 (2 commits)

#### 3.1 config/env.rs
```rust
pub struct EnvConfig {
    pub api_key: String,
    pub callback_url: Option<String>,
    pub port: u16,
    pub tunnel: TunnelConfig,
    pub auto_update: Option<AutoUpdateConfig>,
}

impl EnvConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        // 收拢所有 env::var 调用
    }
}
```

#### 3.2 config/project.rs
- `ProjectConfig`
- `RemoteAgentConfig` + `to_project_config()`

### Phase 4: State 层抽取 (3 commits)

#### 4.1 state/task_store.rs - 解决任务泄漏
```rust
pub struct TaskStore {
    tasks: RwLock<HashMap<String, DeployTask>>,
    history: RwLock<VecDeque<DeployTask>>,
    max_active: usize,      // 默认 50
    max_history: usize,     // 默认 100
    retention: Duration,    // 默认 24h
}

impl TaskStore {
    pub fn create(&self, task: DeployTask) -> String;
    pub fn get(&self, id: &str) -> Option<DeployTask>;
    pub fn update(&self, id: &str, status: DeployStatus, stages: Vec<DeployStage>);
    pub fn finish(&self, id: &str);  // 移到 history + 清理
    pub fn cleanup_stale(&self);     // 定期调用，移除超时任务
}
```

#### 4.2 state/log_hub.rs - 解决 log_channels 泄漏
```rust
pub struct LogHub {
    channels: RwLock<HashMap<String, LogChannel>>,
}

struct LogChannel {
    sender: broadcast::Sender<LogLine>,
    created_at: DateTime<Utc>,
    finished: bool,
}

impl LogHub {
    pub fn create(&self, task_id: &str) -> broadcast::Sender<LogLine>;
    pub fn subscribe(&self, task_id: &str) -> Option<broadcast::Receiver<LogLine>>;
    pub fn finish(&self, task_id: &str);  // 标记完成，让 SSE 知道要结束
    pub fn cleanup(&self);                // 移除已完成的 channel
    pub fn is_finished(&self, task_id: &str) -> bool;
}
```

#### 4.3 state/app_state.rs
重构 `AppState`，组合上述组件：
```rust
pub struct AppState {
    pub config: EnvConfig,
    pub projects: HashMap<String, ProjectConfig>,
    pub task_store: TaskStore,
    pub log_hub: LogHub,
    pub running_deploys: RwLock<HashMap<String, RunningDeploy>>,
    pub deploy_center: DeployCenterClient,
    // tunnel & autoupdate 状态
}
```

### Phase 5: API 层抽取 (6 commits)

每个 commit 抽取一组 handler：

#### 5.1 api/health.rs
- `health_check` -> `/health`, `/status`
- `list_projects` -> `/projects`
- `restart_service` -> `/restart`

#### 5.2 api/deploy.rs
- `trigger_deploy` -> `/trigger/:project`
- `get_task_status` -> `/tasks/:task_id`
- `get_recent_tasks` -> `/tasks/recent`
- `stream_logs` -> `/logs/:task_id/stream`

#### 5.3 api/system.rs
- `get_system_info` -> `/system/info`
- `get_system_stats` -> `/system/stats`

#### 5.4 api/containers.rs
- `list_containers` -> `/containers`
- `get_container_logs` -> `/containers/:name/logs`
- `get_container_env` -> `/containers/:name/env`
- `get_container_env_full` -> `/containers/:name/env/full`

#### 5.5 api/database.rs
- 所有 `/db/*` handlers

#### 5.6 api/tunnel.rs
- `get_tunnel_status` -> `/tunnel/status`
- `tunnel_websocket_handler` -> `/tunnel/ws`

### Phase 6: Services 层抽取 (4 commits)

#### 6.1 services/deploy/context.rs
```rust
pub struct DeployContext {
    pub task_id: String,
    pub project: ProjectConfig,
    pub work_dir: PathBuf,
    pub log_sink: broadcast::Sender<LogLine>,
    pub cancel: CancellationToken,
    pub deploy_center: Arc<DeployCenterClient>,
    pub task_store: Arc<TaskStore>,
}

impl DeployContext {
    pub async fn log(&self, stream: &str, content: &str);
    pub async fn update_status(&self, status: DeployStatus, stages: &[DeployStage]);
    pub async fn finish(&self, status: DeployStatus, stages: Vec<DeployStage>);
}
```

#### 6.2 services/deploy/{script,docker_compose,docker_build,xiaojincut}.rs
每个文件实现：
```rust
pub async fn execute(ctx: DeployContext) -> Result<(), DeployError>;
```

#### 6.3 services/tunnel/{server,client}.rs
抽取隧道逻辑

#### 6.4 services/autoupdate.rs
抽取自动更新逻辑

### Phase 7: Router 组装 (1 commit)

#### 7.1 api/mod.rs
```rust
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .nest("/", health::router())
        .nest("/", deploy::router())
        .nest("/system", system::router())
        .nest("/containers", containers::router())
        .nest("/db", database::router())
        .nest("/tunnel", tunnel::router())
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
```

每个子模块：
```rust
// api/deploy.rs
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/trigger/:project", post(trigger_deploy))
        .route("/tasks/:task_id", get(get_task_status))
        .route("/tasks/recent", get(get_recent_tasks))
        .route("/logs/:task_id/stream", get(stream_logs))
}
```

### Phase 8: main.rs 精简 (1 commit)

最终 main.rs:
```rust
use xjp_deploy_agent::{api, config::EnvConfig, state::AppState};

#[tokio::main]
async fn main() {
    // 1. Tracing
    tracing_subscriber::fmt()
        .json()
        .with_env_filter("xjp_deploy_agent=info")
        .init();

    // 2. Config
    let config = EnvConfig::from_env().expect("Invalid configuration");

    // 3. State
    let state = Arc::new(AppState::new(config).await);

    // 4. Background tasks
    if state.config.tunnel.mode == TunnelMode::Client {
        tokio::spawn(services::tunnel::client::start(state.clone()));
    }
    if let Some(ref update_config) = state.config.auto_update {
        tokio::spawn(services::autoupdate::start(state.clone()));
    }

    // 5. Router
    let app = api::router(state);

    // 6. Server
    let addr = format!("0.0.0.0:{}", config.port);
    tracing::info!("Starting server on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

---

## 关于用户提出的两个问题

### Q1: DB/隧道/自动更新是否做成可选 feature？

**建议：暂不拆分为 feature**

原因：
1. 当前只有一个部署实例使用此代码，feature 增加复杂度但收益有限
2. 这些功能的依赖（sysinfo, tokio-tungstenite）体积不大
3. 可以在 Phase 6 完成后评估，如果需要再拆分

如果将来确实需要：
```toml
[features]
default = ["db", "tunnel", "autoupdate"]
db = []
tunnel = ["tokio-tungstenite"]
autoupdate = []
```

### Q2: 环境变量命名是否兼容旧值？

**建议：保持兼容，但添加新格式**

当前代码实际使用的变量名：
- `DEPLOY_AGENT_API_KEY` (代码使用)
- `DEPLOY_CENTER_CALLBACK_URL` (代码使用)

README 中提到的旧名：
- `API_KEY`
- `CALLBACK_URL`

解决方案 (在 config/env.rs):
```rust
fn load_with_fallback(primary: &str, fallback: &str) -> Option<String> {
    env::var(primary).ok().or_else(|| env::var(fallback).ok())
}

// 使用
let api_key = load_with_fallback("DEPLOY_AGENT_API_KEY", "API_KEY")
    .unwrap_or_else(|| "change-me-in-production".to_string());
```

并在启动时打印 deprecation warning：
```rust
if env::var("API_KEY").is_ok() {
    tracing::warn!("API_KEY is deprecated, use DEPLOY_AGENT_API_KEY instead");
}
```

---

## 预估工作量

| Phase | 描述 | 文件数 | 难度 |
|-------|------|--------|------|
| 1 | 基础设施 | 4 | 低 |
| 2 | Domain 层 | 5 | 低 |
| 3 | Config 层 | 2 | 低 |
| 4 | State 层 | 3 | 中 |
| 5 | API 层 | 6 | 中 |
| 6 | Services 层 | 8 | 高 |
| 7 | Router 组装 | 1 | 低 |
| 8 | main.rs 精简 | 1 | 低 |

**关键路径**：Phase 4 (State 层) 是最关键的一步，因为要解决泄漏问题。

---

## 测试策略

每个 Phase 完成后：
1. `cargo build` 确保编译通过
2. `cargo clippy` 检查 lint
3. 手动测试：
   - `curl http://localhost:9876/health`
   - 触发一次部署
   - 检查 SSE 日志流

---

## 回滚策略

每个 Phase 是独立 commit，如果出问题可以 `git revert`。

建议在开始前创建分支：
```bash
git checkout -b refactor/modularize-main
```
