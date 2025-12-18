//! XJP Deploy Agent - 私有云部署代理
//!
//! 运行在私有云上，接收部署中心的触发请求，执行构建脚本并流式传输日志。

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    env,
    process::Stdio,
    sync::Arc,
};
use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::Command,
    sync::{broadcast, mpsc, RwLock},
    time::Duration,
};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as TungsteniteMessage};
use futures::{SinkExt, StreamExt};
use tokio_util::sync::CancellationToken;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info, warn};
use uuid::Uuid;

/// 部署任务状态
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployStatus {
    Running,
    Success,
    Failed,
}

/// 阶段状态
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Pending,
    Running,
    Success,
    Failed,
    Skipped,
}

/// 部署阶段信息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployStage {
    /// 阶段标识 (e.g., "git_pull", "docker_build", "docker_push")
    pub name: String,
    /// 显示名称 (e.g., "Git Pull", "Docker Build")
    pub display_name: String,
    /// 开始时间
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 结束时间
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 持续时间（毫秒）
    pub duration_ms: Option<i64>,
    /// 阶段状态
    pub status: StageStatus,
    /// 附加信息
    pub message: Option<String>,
}

impl DeployStage {
    pub fn new(name: &str, display_name: &str) -> Self {
        Self {
            name: name.to_string(),
            display_name: display_name.to_string(),
            started_at: None,
            finished_at: None,
            duration_ms: None,
            status: StageStatus::Pending,
            message: None,
        }
    }

    pub fn start(&mut self) {
        self.started_at = Some(chrono::Utc::now());
        self.status = StageStatus::Running;
    }

    pub fn finish(&mut self, success: bool, message: Option<String>) {
        let now = chrono::Utc::now();
        self.finished_at = Some(now);
        self.status = if success { StageStatus::Success } else { StageStatus::Failed };
        self.message = message;
        if let Some(started) = self.started_at {
            self.duration_ms = Some((now - started).num_milliseconds());
        }
    }

    pub fn skip(&mut self, reason: Option<String>) {
        self.status = StageStatus::Skipped;
        self.message = reason;
    }
}

/// 部署任务信息
#[derive(Clone, Debug, Serialize)]
pub struct DeployTask {
    pub id: String,
    pub project: String,
    pub status: DeployStatus,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub exit_code: Option<i32>,
    /// 部署阶段详情
    #[serde(default)]
    pub stages: Vec<DeployStage>,
}

/// 日志行
#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub stream: String, // stdout | stderr
    pub content: String,
}

/// 部署类型
#[derive(Clone, Debug)]
pub enum DeployType {
    /// 执行脚本（现有方式）
    Script { script: String },
    /// Docker Compose 部署（拉取镜像并重启）
    DockerCompose {
        /// docker-compose.yml 文件路径（相对于 work_dir 或绝对路径）
        compose_file: String,
        /// 要拉取的镜像（如 ghcr.io/rickyjim626/xjp-backend:latest）
        image: String,
        /// Git 仓库目录（用于 git pull 更新配置，可选）
        git_repo_dir: Option<String>,
        /// 要重启的服务名称（如果不指定则重启所有服务）
        service: Option<String>,
    },
    /// Docker Build 部署（拉取代码、构建镜像、推送到 registry）
    DockerBuild {
        /// Dockerfile 路径（相对于 work_dir，默认 "Dockerfile"）
        dockerfile: Option<String>,
        /// 构建上下文路径（相对于 work_dir，默认 "."）
        build_context: Option<String>,
        /// 目标镜像名称（包含 registry，如 ghcr.io/user/repo）
        image: String,
        /// 镜像 tag（默认使用 git commit hash）
        tag: Option<String>,
        /// 是否同时推送 latest tag
        push_latest: bool,
        /// Git 分支（默认 master）
        branch: Option<String>,
    },
    /// Xiaojincut 本地视频处理任务
    Xiaojincut {
        /// 任务类型: "import", "export", "process"
        task_type: String,
        /// 输入文件路径
        input_path: Option<String>,
        /// 输出目录
        output_dir: Option<String>,
        /// AI 提示（用于 AI 处理任务）
        prompt: Option<String>,
        /// xiaojincut 服务端口（默认 19527）
        port: Option<u16>,
    },
}

/// 项目配置
#[derive(Clone, Debug)]
pub struct ProjectConfig {
    pub work_dir: String,
    pub deploy_type: DeployType,
}

// ============== 远程配置（从部署中心获取）==============

/// 远程 Agent 项目配置（来自部署中心 API）
#[derive(Clone, Debug, Deserialize)]
pub struct RemoteAgentConfig {
    /// Deploy type: "execute_command", "docker_build", "docker_compose"
    #[serde(default = "default_deploy_type")]
    pub deploy_type: String,
    /// Script or command to execute (for execute_command type)
    pub script: Option<String>,
    /// Working directory
    pub work_dir: Option<String>,
    /// Dockerfile path (for docker_build type)
    pub dockerfile: Option<String>,
    /// Docker build context path
    pub build_context: Option<String>,
    /// Docker image name
    pub image: Option<String>,
    /// Docker image tag
    pub tag: Option<String>,
    /// Also push with :latest tag
    #[serde(default)]
    pub push_latest: bool,
    /// Git branch
    pub branch: Option<String>,
    /// Git repository URL
    pub repo_url: Option<String>,
    /// docker-compose.yml path (for docker_compose type)
    pub compose_file: Option<String>,
    /// Service name to restart (for docker_compose type)
    pub services: Option<Vec<String>>,
    /// Environment variables
    pub env: Option<HashMap<String, String>>,
    /// Timeout in seconds
    pub timeout_secs: Option<u32>,
    // Xiaojincut 相关字段
    /// Xiaojincut 任务类型: "import", "export", "process"
    pub xjc_task_type: Option<String>,
    /// Xiaojincut 输入文件路径
    pub xjc_input_path: Option<String>,
    /// Xiaojincut 输出目录
    pub xjc_output_dir: Option<String>,
    /// Xiaojincut AI 提示
    pub xjc_prompt: Option<String>,
    /// Xiaojincut 服务端口
    pub xjc_port: Option<u16>,
}

fn default_deploy_type() -> String {
    "execute_command".to_string()
}

/// 远程配置响应
#[derive(Clone, Debug, Deserialize)]
pub struct RemoteAgentConfigResponse {
    pub project_slug: String,
    pub config: RemoteAgentConfig,
    pub project_name: String,
    pub repo_owner: String,
    pub repo_name: String,
    pub branch: String,
}

impl RemoteAgentConfig {
    /// 转换为本地 ProjectConfig
    fn to_project_config(&self) -> Option<ProjectConfig> {
        let work_dir = self.work_dir.clone()?;

        let deploy_type = match self.deploy_type.as_str() {
            "docker_build" | "docker-build" => {
                let image = self.image.clone()?;
                DeployType::DockerBuild {
                    dockerfile: self.dockerfile.clone(),
                    build_context: self.build_context.clone(),
                    image,
                    tag: self.tag.clone(),
                    push_latest: self.push_latest,
                    branch: self.branch.clone(),
                }
            }
            "docker_compose" | "docker-compose" => {
                let image = self.image.clone()?;
                DeployType::DockerCompose {
                    compose_file: self.compose_file.clone().unwrap_or_else(|| "docker-compose.yml".to_string()),
                    image,
                    git_repo_dir: Some(work_dir.clone()),
                    service: self.services.as_ref().and_then(|s| s.first().cloned()),
                }
            }
            "xiaojincut" => {
                DeployType::Xiaojincut {
                    task_type: self.xjc_task_type.clone().unwrap_or_else(|| "import".to_string()),
                    input_path: self.xjc_input_path.clone(),
                    output_dir: self.xjc_output_dir.clone(),
                    prompt: self.xjc_prompt.clone(),
                    port: self.xjc_port,
                }
            }
            _ => {
                // execute_command / script
                let script = self.script.clone().unwrap_or_else(|| "./deploy.sh".to_string());
                DeployType::Script { script }
            }
        };

        Some(ProjectConfig { work_dir, deploy_type })
    }
}

// ============== 远程配置结束 ==============

/// 系统配置信息
#[derive(Clone, Debug, Serialize)]
pub struct SystemInfo {
    pub hostname: String,
    pub os_name: String,
    pub os_version: String,
    pub kernel_version: String,
    pub cpu_arch: String,
    pub cpu_count: usize,
    pub cpu_brand: String,
    pub total_memory_gb: f64,
    pub total_swap_gb: f64,
}

/// 磁盘信息
#[derive(Clone, Debug, Serialize)]
pub struct DiskInfo {
    pub name: String,
    pub mount_point: String,
    pub total_gb: f64,
    pub used_gb: f64,
    pub available_gb: f64,
    pub usage_percent: f64,
}

/// 系统负载统计
#[derive(Clone, Debug, Serialize)]
pub struct SystemStats {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub cpu_usage_percent: f64,
    pub memory_used_gb: f64,
    pub memory_total_gb: f64,
    pub memory_usage_percent: f64,
    pub swap_used_gb: f64,
    pub swap_total_gb: f64,
    pub swap_usage_percent: f64,
    pub disks: Vec<DiskInfo>,
    pub load_average: LoadAverage,
}

/// 系统负载平均值 (1, 5, 15 分钟)
#[derive(Clone, Debug, Serialize)]
pub struct LoadAverage {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

/// 任务历史查询参数
#[derive(Debug, Deserialize)]
pub struct TaskHistoryQuery {
    /// 返回数量限制，默认 20
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// 项目名称过滤
    pub project: Option<String>,
    /// 状态过滤 (success, failed, running)
    pub status: Option<String>,
}

fn default_limit() -> usize {
    20
}

/// 任务历史响应
#[derive(Debug, Serialize)]
pub struct TaskHistoryResponse {
    pub tasks: Vec<DeployTask>,
    pub total: usize,
}

/// 容器信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub state: String,
    pub created: String,
    pub ports: Vec<String>,
}

/// 容器列表响应
#[derive(Debug, Serialize)]
pub struct ContainersResponse {
    pub containers: Vec<ContainerInfo>,
}

/// 容器日志查询参数
#[derive(Debug, Deserialize)]
pub struct ContainerLogsQuery {
    /// 返回最后 N 行，默认 100
    #[serde(default = "default_log_lines")]
    pub tail: usize,
    /// 是否显示时间戳
    #[serde(default)]
    pub timestamps: bool,
    /// 只显示最近 N 秒的日志
    pub since: Option<String>,
}

fn default_log_lines() -> usize {
    100
}

/// 容器日志响应
#[derive(Debug, Serialize)]
pub struct ContainerLogsResponse {
    pub container: String,
    pub logs: Vec<String>,
    pub total_lines: usize,
}

/// 环境变量信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
    /// 是否是敏感信息（密码、密钥等）
    #[serde(default)]
    pub sensitive: bool,
}

/// 容器环境变量响应
#[derive(Debug, Serialize)]
pub struct ContainerEnvResponse {
    pub container: String,
    pub env_vars: Vec<EnvVar>,
}

// ============== 数据库管理相关结构 ==============

/// 数据库类型
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseType {
    Postgres,
    Mysql,
}

impl std::fmt::Display for DatabaseType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DatabaseType::Postgres => write!(f, "postgres"),
            DatabaseType::Mysql => write!(f, "mysql"),
        }
    }
}

/// 数据库连接配置
#[derive(Debug, Clone, Deserialize)]
pub struct DbConnectionConfig {
    /// 数据库类型
    pub db_type: DatabaseType,
    /// Docker 容器名称
    pub container: String,
    /// 数据库名称
    pub database: String,
    /// 用户名
    #[serde(default)]
    pub username: Option<String>,
    /// 密码
    #[serde(default)]
    pub password: Option<String>,
}

/// 列出数据库请求
#[derive(Debug, Deserialize)]
pub struct ListDatabasesRequest {
    /// 数据库类型过滤
    pub db_type: Option<DatabaseType>,
}

/// 数据库信息
#[derive(Debug, Serialize)]
pub struct DatabaseInfo {
    pub name: String,
    pub db_type: DatabaseType,
    pub container: String,
    pub size_mb: Option<f64>,
}

/// 列出数据库响应
#[derive(Debug, Serialize)]
pub struct ListDatabasesResponse {
    pub databases: Vec<DatabaseInfo>,
}

/// 列出表请求
#[derive(Debug, Deserialize)]
pub struct ListTablesRequest {
    pub connection: DbConnectionConfig,
    /// Schema 名称 (PostgreSQL 用，默认 public)
    #[serde(default)]
    pub schema: Option<String>,
}

/// 表信息
#[derive(Debug, Serialize)]
pub struct TableInfo {
    pub name: String,
    pub schema: Option<String>,
    pub row_count: Option<i64>,
    pub size_kb: Option<f64>,
}

/// 列出表响应
#[derive(Debug, Serialize)]
pub struct ListTablesResponse {
    pub tables: Vec<TableInfo>,
    pub database: String,
}

/// 获取表结构请求
#[derive(Debug, Deserialize)]
pub struct GetSchemaRequest {
    pub connection: DbConnectionConfig,
    pub table: String,
    #[serde(default)]
    pub schema: Option<String>,
}

/// 列信息
#[derive(Debug, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub is_nullable: bool,
    pub default_value: Option<String>,
    pub is_primary_key: bool,
}

/// 索引信息
#[derive(Debug, Serialize)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
}

/// 获取表结构响应
#[derive(Debug, Serialize)]
pub struct GetSchemaResponse {
    pub table: String,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub row_count: Option<i64>,
}

/// 执行查询请求
#[derive(Debug, Deserialize)]
pub struct DbQueryRequest {
    pub connection: DbConnectionConfig,
    pub sql: String,
    /// 最大返回行数，默认 1000
    #[serde(default = "default_max_rows")]
    pub max_rows: usize,
    /// 查询超时秒数，默认 30
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
}

fn default_max_rows() -> usize {
    1000
}

fn default_timeout() -> u32 {
    30
}

/// 执行查询响应
#[derive(Debug, Serialize)]
pub struct DbQueryResponse {
    pub success: bool,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: usize,
    pub execution_time_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// 执行命令请求 (INSERT/UPDATE/DELETE)
#[derive(Debug, Deserialize)]
pub struct DbExecuteRequest {
    pub connection: DbConnectionConfig,
    pub sql: String,
    /// 确认危险操作
    #[serde(default)]
    pub confirm_dangerous: bool,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
}

/// 执行命令响应
#[derive(Debug, Serialize)]
pub struct DbExecuteResponse {
    pub success: bool,
    pub affected_rows: i64,
    pub execution_time_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 导出格式
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Csv,
    Json,
    Sql,
}

/// 导出数据请求
#[derive(Debug, Deserialize)]
pub struct DbExportRequest {
    pub connection: DbConnectionConfig,
    /// 表名或 SQL 查询
    pub source: String,
    /// source 是否为 SQL 查询
    #[serde(default)]
    pub is_query: bool,
    #[serde(default = "default_export_format")]
    pub format: ExportFormat,
    /// 最大导出行数，默认 100000
    #[serde(default = "default_export_max_rows")]
    pub max_rows: usize,
}

fn default_export_format() -> ExportFormat {
    ExportFormat::Csv
}

fn default_export_max_rows() -> usize {
    100000
}

/// 导出数据响应
#[derive(Debug, Serialize)]
pub struct DbExportResponse {
    pub success: bool,
    pub format: ExportFormat,
    pub row_count: usize,
    pub data: String,
    pub execution_time_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

// ============== 数据库管理结束 ==============

// ============== 隧道 (FRP) 相关结构 ==============

/// 隧道运行模式
#[derive(Clone, Debug, PartialEq)]
pub enum TunnelMode {
    /// 服务端模式 (ECS): 接收隧道连接，暴露远程服务
    Server,
    /// 客户端模式 (私有云): 主动连接服务端，转发本地服务
    Client,
    /// 禁用隧道
    Disabled,
}

impl TunnelMode {
    fn from_env() -> Self {
        match env::var("TUNNEL_MODE").as_deref() {
            Ok("server") => TunnelMode::Server,
            Ok("client") => TunnelMode::Client,
            _ => TunnelMode::Disabled,
        }
    }
}

/// 端口映射配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortMapping {
    /// 远程端口 (在 ECS 上暴露)
    pub remote_port: u16,
    /// 本地主机
    pub local_host: String,
    /// 本地端口
    pub local_port: u16,
    /// 服务名称 (如 "minio", "postgres")
    pub name: String,
}

impl PortMapping {
    /// 从环境变量解析端口映射
    /// 格式: "name:remote_port:local_host:local_port,..."
    /// 例如: "minio:19000:localhost:9000,postgres:15432:localhost:5432"
    fn from_env() -> Vec<Self> {
        let mappings_str = env::var("TUNNEL_MAPPINGS").unwrap_or_default();
        mappings_str
            .split(',')
            .filter_map(|s| {
                let parts: Vec<&str> = s.trim().split(':').collect();
                if parts.len() == 4 {
                    Some(PortMapping {
                        name: parts[0].to_string(),
                        remote_port: parts[1].parse().ok()?,
                        local_host: parts[2].to_string(),
                        local_port: parts[3].parse().ok()?,
                    })
                } else {
                    None
                }
            })
            .collect()
    }
}

/// 隧道协议消息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TunnelMessage {
    /// 客户端发送端口映射配置
    Config { mappings: Vec<PortMapping> },
    /// 新连接请求 (Server -> Client)
    Connect { conn_id: String, remote_port: u16 },
    /// 连接已建立 (Client -> Server)
    Connected { conn_id: String },
    /// 连接失败 (Client -> Server)
    ConnectFailed { conn_id: String, error: String },
    /// 数据传输
    Data { conn_id: String, data: Vec<u8> },
    /// 连接关闭
    Close { conn_id: String },
    /// 心跳
    Ping,
    Pong,
}

/// 隧道连接信息 (用于 API 响应)
#[derive(Clone, Debug, Serialize)]
pub struct TunnelConnectionInfo {
    pub id: String,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub remote_addr: String,
    pub mappings: Vec<PortMapping>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// 端口映射状态信息
#[derive(Clone, Debug, Serialize)]
pub struct PortMappingStatus {
    pub mapping: PortMapping,
    pub status: String, // "active", "listening", "error"
    pub active_connections: usize,
}

/// 客户端连接状态
#[derive(Clone, Debug, Serialize)]
pub struct TunnelClientStatus {
    pub connected: bool,
    pub server_url: String,
    pub connected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    pub reconnect_count: u32,
    pub mappings: Vec<PortMappingStatus>,
}

/// 服务端状态
#[derive(Clone, Debug, Serialize)]
pub struct TunnelServerStatus {
    pub listening: bool,
    pub listen_port: u16,
    pub client_connected: bool,
    pub client_addr: Option<String>,
    pub client_connected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub port_mappings: Vec<PortMappingStatus>,
}

/// 隧道状态响应
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "mode")]
pub enum TunnelStatusResponse {
    #[serde(rename = "server")]
    Server(TunnelServerStatus),
    #[serde(rename = "client")]
    Client(TunnelClientStatus),
    #[serde(rename = "disabled")]
    Disabled,
}

/// 隧道状态 (Server 模式)
pub struct TunnelServerState {
    /// 客户端 WebSocket 连接是否活跃
    pub client_connected: RwLock<bool>,
    /// 客户端地址
    pub client_addr: RwLock<Option<String>>,
    /// 客户端连接时间
    pub client_connected_at: RwLock<Option<chrono::DateTime<chrono::Utc>>>,
    /// 客户端的端口映射配置
    pub client_mappings: RwLock<Vec<PortMapping>>,
    /// 活跃的代理连接 (conn_id -> 发送通道)
    pub proxy_connections: RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>,
    /// 发送消息到 WebSocket 客户端的通道
    pub ws_tx: RwLock<Option<mpsc::Sender<TunnelMessage>>>,
    /// 端口监听器取消令牌 (port -> cancel_token)
    pub port_listeners: RwLock<HashMap<u16, CancellationToken>>,
}

impl TunnelServerState {
    fn new() -> Self {
        Self {
            client_connected: RwLock::new(false),
            client_addr: RwLock::new(None),
            client_connected_at: RwLock::new(None),
            client_mappings: RwLock::new(Vec::new()),
            proxy_connections: RwLock::new(HashMap::new()),
            ws_tx: RwLock::new(None),
            port_listeners: RwLock::new(HashMap::new()),
        }
    }
}

/// 隧道状态 (Client 模式)
pub struct TunnelClientState {
    /// 是否已连接
    pub connected: RwLock<bool>,
    /// 连接时间
    pub connected_at: RwLock<Option<chrono::DateTime<chrono::Utc>>>,
    /// 最后错误
    pub last_error: RwLock<Option<String>>,
    /// 重连次数
    pub reconnect_count: RwLock<u32>,
    /// 活跃的本地连接 (conn_id -> 发送通道)
    pub local_connections: RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>,
}

impl TunnelClientState {
    fn new() -> Self {
        Self {
            connected: RwLock::new(false),
            connected_at: RwLock::new(None),
            last_error: RwLock::new(None),
            reconnect_count: RwLock::new(0),
            local_connections: RwLock::new(HashMap::new()),
        }
    }
}

// ============== 隧道结构结束 ==============

// ============== 自动更新结构 ==============

/// 更新元数据 (从 latest.json 解析)
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpdateMetadata {
    pub version: String,
    pub released_at: String,
    pub changelog: Option<String>,
    pub sha256: Option<String>,
}

/// 自动更新配置
#[derive(Clone, Debug)]
pub struct AutoUpdateConfig {
    /// 是否启用
    pub enabled: bool,
    /// 存储端点 (如 https://rickyjim.oss-cn-shanghai-internal.aliyuncs.com)
    pub endpoint: String,
    /// 元数据路径 (如 releases/xjp-deploy-agent/latest.json)
    pub metadata_path: String,
    /// 二进制路径模板 (如 releases/xjp-deploy-agent/xjp-deploy-agent-{version}-linux-musl-amd64)
    pub binary_path_template: String,
    /// 检查间隔（秒）
    pub check_interval_secs: u64,
}

impl AutoUpdateConfig {
    fn from_env() -> Option<Self> {
        let enabled = env::var("AUTO_UPDATE_ENABLED")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        if !enabled {
            return None;
        }

        let endpoint = env::var("UPDATE_ENDPOINT").ok()?;
        let metadata_path = env::var("UPDATE_METADATA_PATH").ok()?;
        let binary_path_template = env::var("UPDATE_BINARY_PATH_TEMPLATE").ok()?;
        let check_interval_secs = env::var("UPDATE_CHECK_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600);

        Some(Self {
            enabled,
            endpoint,
            metadata_path,
            binary_path_template,
            check_interval_secs,
        })
    }
}

/// 自动更新状态
pub struct AutoUpdateState {
    /// 上次检查时间
    pub last_check: RwLock<Option<chrono::DateTime<chrono::Utc>>>,
    /// 上次检查结果
    pub last_check_result: RwLock<Option<String>>,
    /// 检测到的最新版本
    pub latest_version: RwLock<Option<String>>,
    /// 是否有可用更新
    pub update_available: RwLock<bool>,
    /// 更新进度 (downloading, verifying, applying, none)
    pub update_progress: RwLock<String>,
    /// 下一次检查时间
    pub next_check: RwLock<Option<chrono::DateTime<chrono::Utc>>>,
}

impl AutoUpdateState {
    fn new() -> Self {
        Self {
            last_check: RwLock::new(None),
            last_check_result: RwLock::new(None),
            latest_version: RwLock::new(None),
            update_available: RwLock::new(false),
            update_progress: RwLock::new("none".to_string()),
            next_check: RwLock::new(None),
        }
    }
}

/// 自动更新状态响应（用于 health 接口）
#[derive(Clone, Debug, Serialize)]
pub struct AutoUpdateStatus {
    pub enabled: bool,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub last_check: Option<String>,
    pub last_check_result: Option<String>,
    pub next_check: Option<String>,
    pub update_progress: String,
}

// ============== 自动更新结构结束 ==============

/// 运行中的部署进程信息
pub struct RunningDeploy {
    pub task_id: String,
    pub cancel_token: CancellationToken,
}

/// 任务历史最大保存数量
const MAX_TASK_HISTORY: usize = 100;

/// 应用状态
pub struct AppState {
    /// API 密钥（用于验证请求）
    api_key: String,
    /// 项目配置
    projects: HashMap<String, ProjectConfig>,
    /// 活跃的部署任务
    tasks: RwLock<HashMap<String, DeployTask>>,
    /// 日志广播通道 (task_id -> sender)
    log_channels: RwLock<HashMap<String, broadcast::Sender<LogLine>>>,
    /// 回调 URL（可选，用于通知部署中心）
    callback_url: Option<String>,
    /// 每个项目当前运行中的部署 (project -> RunningDeploy)
    running_deploys: RwLock<HashMap<String, RunningDeploy>>,
    /// 任务历史记录 (最近 MAX_TASK_HISTORY 个已完成的任务)
    task_history: RwLock<VecDeque<DeployTask>>,
    /// 服务启动时间
    started_at: chrono::DateTime<chrono::Utc>,
    // ========== 隧道相关字段 ==========
    /// 隧道模式
    tunnel_mode: TunnelMode,
    /// 隧道认证令牌
    tunnel_auth_token: String,
    /// 隧道服务端状态 (仅 Server 模式)
    tunnel_server_state: Arc<TunnelServerState>,
    /// 隧道客户端状态 (仅 Client 模式)
    tunnel_client_state: Arc<TunnelClientState>,
    /// 端口映射配置 (Client 模式)
    tunnel_port_mappings: Vec<PortMapping>,
    /// 隧道服务端 URL (Client 模式)
    tunnel_server_url: String,
    // ========== 自动更新相关字段 ==========
    /// 自动更新配置
    auto_update_config: Option<AutoUpdateConfig>,
    /// 自动更新状态
    auto_update_state: Arc<AutoUpdateState>,
}

impl AppState {
    fn new() -> Self {
        let api_key = env::var("DEPLOY_AGENT_API_KEY")
            .unwrap_or_else(|_| "change-me-in-production".to_string());

        let callback_url = env::var("DEPLOY_CENTER_CALLBACK_URL").ok();

        // 配置项目
        let mut projects = HashMap::new();

        // xiaojinpro-backend (只在 BACKEND_WORK_DIR 存在时添加)
        if let Ok(work_dir) = env::var("BACKEND_WORK_DIR") {
            projects.insert(
                "xiaojinpro-backend".to_string(),
                ProjectConfig {
                    work_dir,
                    deploy_type: DeployType::Script {
                        script: "./build-and-push-backend.sh".to_string(),
                    },
                },
            );
        }

        // 从环境变量添加更多项目
        // 脚本类型: PROJECT_<NAME>_DIR, PROJECT_<NAME>_SCRIPT
        // Docker Compose 类型: PROJECT_<NAME>_DIR, PROJECT_<NAME>_TYPE=docker_compose,
        //                      PROJECT_<NAME>_IMAGE, PROJECT_<NAME>_COMPOSE_FILE, PROJECT_<NAME>_GIT_REPO
        for (key, value) in env::vars() {
            if key.starts_with("PROJECT_") && key.ends_with("_DIR") {
                let name = key
                    .strip_prefix("PROJECT_")
                    .unwrap()
                    .strip_suffix("_DIR")
                    .unwrap()
                    .to_lowercase()
                    .replace('_', "-");

                let prefix = format!("PROJECT_{}", name.to_uppercase().replace('-', "_"));
                let type_key = format!("{}_TYPE", prefix);
                let deploy_type_str = env::var(&type_key).unwrap_or_else(|_| "script".to_string());

                let deploy_type = match deploy_type_str.as_str() {
                    "docker_compose" | "docker-compose" => {
                        let image_key = format!("{}_IMAGE", prefix);
                        let compose_key = format!("{}_COMPOSE_FILE", prefix);
                        let git_repo_key = format!("{}_GIT_REPO", prefix);

                        let image = env::var(&image_key).unwrap_or_else(|_| {
                            warn!("Missing {} for docker_compose project {}", image_key, name);
                            "".to_string()
                        });
                        let compose_file = env::var(&compose_key)
                            .unwrap_or_else(|_| "docker-compose.yml".to_string());
                        let git_repo_dir = env::var(&git_repo_key).ok();
                        let service_key = format!("{}_SERVICE", prefix);
                        let service = env::var(&service_key).ok();

                        DeployType::DockerCompose {
                            compose_file,
                            image,
                            git_repo_dir,
                            service,
                        }
                    }
                    "docker_build" | "docker-build" => {
                        let image_key = format!("{}_IMAGE", prefix);
                        let dockerfile_key = format!("{}_DOCKERFILE", prefix);
                        let context_key = format!("{}_BUILD_CONTEXT", prefix);
                        let tag_key = format!("{}_TAG", prefix);
                        let push_latest_key = format!("{}_PUSH_LATEST", prefix);
                        let branch_key = format!("{}_BRANCH", prefix);

                        let image = env::var(&image_key).unwrap_or_else(|_| {
                            warn!("Missing {} for docker_build project {}", image_key, name);
                            "".to_string()
                        });
                        let dockerfile = env::var(&dockerfile_key).ok();
                        let build_context = env::var(&context_key).ok();
                        let tag = env::var(&tag_key).ok();
                        let push_latest = env::var(&push_latest_key)
                            .map(|v| v == "true" || v == "1")
                            .unwrap_or(true);
                        let branch = env::var(&branch_key).ok();

                        DeployType::DockerBuild {
                            dockerfile,
                            build_context,
                            image,
                            tag,
                            push_latest,
                            branch,
                        }
                    }
                    "xiaojincut" => {
                        let task_type_key = format!("{}_TASK_TYPE", prefix);
                        let input_path_key = format!("{}_INPUT_PATH", prefix);
                        let output_dir_key = format!("{}_OUTPUT_DIR", prefix);
                        let prompt_key = format!("{}_PROMPT", prefix);
                        let port_key = format!("{}_PORT", prefix);

                        let task_type = env::var(&task_type_key).unwrap_or_else(|_| "import".to_string());
                        let input_path = env::var(&input_path_key).ok();
                        let output_dir = env::var(&output_dir_key).ok();
                        let prompt = env::var(&prompt_key).ok();
                        let port = env::var(&port_key).ok().and_then(|p| p.parse().ok());

                        DeployType::Xiaojincut {
                            task_type,
                            input_path,
                            output_dir,
                            prompt,
                            port,
                        }
                    }
                    _ => {
                        let script_key = format!("{}_SCRIPT", prefix);
                        let script = env::var(&script_key).unwrap_or_else(|_| "./deploy.sh".to_string());
                        DeployType::Script { script }
                    }
                };

                projects.insert(
                    name.clone(),
                    ProjectConfig {
                        work_dir: value,
                        deploy_type,
                    },
                );
                info!("Loaded project config: {} ({:?})", name, deploy_type_str);
            }
        }

        // 隧道配置
        let tunnel_mode = TunnelMode::from_env();
        let tunnel_auth_token = env::var("TUNNEL_AUTH_TOKEN")
            .unwrap_or_else(|_| "change-tunnel-token".to_string());
        let tunnel_server_url = env::var("TUNNEL_SERVER_URL")
            .unwrap_or_else(|_| "ws://localhost:9878/tunnel/ws".to_string());
        let tunnel_port_mappings = PortMapping::from_env();

        if tunnel_mode != TunnelMode::Disabled {
            info!(
                "Tunnel mode: {:?}, mappings: {:?}",
                tunnel_mode, tunnel_port_mappings
            );
        }

        // 自动更新配置
        let auto_update_config = AutoUpdateConfig::from_env();
        if let Some(ref config) = auto_update_config {
            info!(
                "Auto-update enabled: check interval {}s, endpoint: {}",
                config.check_interval_secs, config.endpoint
            );
        }

        Self {
            api_key,
            projects,
            tasks: RwLock::new(HashMap::new()),
            log_channels: RwLock::new(HashMap::new()),
            callback_url,
            running_deploys: RwLock::new(HashMap::new()),
            task_history: RwLock::new(VecDeque::with_capacity(MAX_TASK_HISTORY)),
            started_at: chrono::Utc::now(),
            // 隧道
            tunnel_mode,
            tunnel_auth_token,
            tunnel_server_state: Arc::new(TunnelServerState::new()),
            tunnel_client_state: Arc::new(TunnelClientState::new()),
            tunnel_port_mappings,
            tunnel_server_url,
            // 自动更新
            auto_update_config,
            auto_update_state: Arc::new(AutoUpdateState::new()),
        }
    }

    /// 添加任务到历史记录
    async fn add_to_history(&self, task: DeployTask) {
        let mut history = self.task_history.write().await;
        // 如果已满，移除最旧的
        if history.len() >= MAX_TASK_HISTORY {
            history.pop_front();
        }
        history.push_back(task);
    }

    /// 验证 API 密钥
    fn verify_api_key(&self, headers: &HeaderMap) -> bool {
        headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(|k| k == self.api_key)
            .unwrap_or(false)
    }
}

/// 触发部署请求
#[derive(Debug, Deserialize)]
pub struct TriggerRequest {
    /// 部署日志 ID（来自部署中心）
    pub deploy_log_id: Option<String>,
    /// 提交哈希
    pub commit_hash: Option<String>,
    /// 分支
    pub branch: Option<String>,
}

/// 触发部署响应
#[derive(Debug, Serialize)]
pub struct TriggerResponse {
    pub task_id: String,
    pub project: String,
    pub status: String,
    pub stream_url: String,
}

/// 错误响应
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
}

/// 版本信息
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 重启响应
#[derive(Debug, Serialize)]
pub struct RestartResponse {
    pub success: bool,
    pub message: String,
}

/// 健康检查 - 返回状态、版本、运行时间等信息
async fn health_check(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let (active_count, active_projects) = {
        let running_deploys = state.running_deploys.read().await;
        let count = running_deploys.len();
        let projects: Vec<String> = running_deploys.keys().cloned().collect();
        (count, projects)
    };

    let projects: Vec<String> = state.projects.keys().cloned().collect();

    // 构建自动更新状态
    let auto_update_status = if let Some(ref _config) = state.auto_update_config {
        let update_state = &state.auto_update_state;
        AutoUpdateStatus {
            enabled: true,
            current_version: VERSION.to_string(),
            latest_version: update_state.latest_version.read().await.clone(),
            update_available: *update_state.update_available.read().await,
            last_check: update_state.last_check.read().await.map(|t| t.to_rfc3339()),
            last_check_result: update_state.last_check_result.read().await.clone(),
            next_check: update_state.next_check.read().await.map(|t| t.to_rfc3339()),
            update_progress: update_state.update_progress.read().await.clone(),
        }
    } else {
        // 回退到旧的逻辑：检查是否配置了 xjp-deploy-agent 项目
        let legacy_enabled = state.projects.contains_key("xjp-deploy-agent");
        AutoUpdateStatus {
            enabled: legacy_enabled,
            current_version: VERSION.to_string(),
            latest_version: None,
            update_available: false,
            last_check: None,
            last_check_result: None,
            next_check: None,
            update_progress: "none".to_string(),
        }
    };

    Json(serde_json::json!({
        "status": "ok",
        "service": "xjp-deploy-agent",
        "version": VERSION,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "projects": projects,
        "active_deploys": active_count,
        "active_projects": active_projects,
        "auto_update_enabled": auto_update_status.enabled,
        "auto_update": auto_update_status
    }))
}

/// 列出支持的项目
async fn list_projects(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    let projects: Vec<&String> = state.projects.keys().collect();
    Ok(Json(serde_json::json!({ "projects": projects })))
}

/// 重启服务 (通过 systemd-run 延迟重启)
///
/// POST /restart
async fn restart_service(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<RestartResponse>, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    info!("Restart requested, scheduling service restart in 2 seconds...");

    // 使用 systemd-run 延迟 2 秒重启服务
    let result = Command::new("sudo")
        .args([
            "/usr/bin/systemd-run",
            "--no-block",
            "--on-active=2s",
            "/bin/systemctl",
            "restart",
            "xjp-deploy-agent",
        ])
        .output()
        .await;

    match result {
        Ok(output) => {
            if output.status.success() {
                info!("Service restart scheduled successfully");
                Ok(Json(RestartResponse {
                    success: true,
                    message: "Service restart scheduled in 2 seconds".to_string(),
                }))
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                error!("Failed to schedule restart: {}", stderr);
                Ok(Json(RestartResponse {
                    success: false,
                    message: format!("Failed to schedule restart: {}", stderr),
                }))
            }
        }
        Err(e) => {
            error!("Failed to run systemd-run: {}", e);
            Ok(Json(RestartResponse {
                success: false,
                message: format!("Failed to run restart command: {}", e),
            }))
        }
    }
}

/// 触发部署
async fn trigger_deploy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Json(request): Json<TriggerRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    // 验证 API 密钥
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    // 获取项目配置
    // 优先从部署中心获取配置，如果失败则使用本地环境变量配置
    let config: ProjectConfig = if let Some(ref callback_url) = state.callback_url {
        // 尝试从部署中心获取配置
        if let Some(remote_config) = fetch_project_config_from_center(callback_url, &project).await {
            info!(project = %project, "Using config from deploy center");
            remote_config
        } else if let Some(local_config) = state.projects.get(&project) {
            info!(project = %project, "Using local config (deploy center config not available)");
            local_config.clone()
        } else {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "not_found".to_string(),
                    message: format!("Project '{}' not configured (no remote or local config)", project),
                }),
            ));
        }
    } else if let Some(local_config) = state.projects.get(&project) {
        info!(project = %project, "Using local config (no deploy center configured)");
        local_config.clone()
    } else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".to_string(),
                message: format!("Project '{}' not configured", project),
            }),
        ));
    };

    let task_id = request
        .deploy_log_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // 检查是否有该项目正在运行的部署，如果有则取消它
    {
        let mut running = state.running_deploys.write().await;
        if let Some(old_deploy) = running.remove(&project) {
            warn!(
                project = %project,
                old_task_id = %old_deploy.task_id,
                new_task_id = %task_id,
                "Cancelling previous deployment for new commit"
            );
            // 触发取消
            old_deploy.cancel_token.cancel();

            // 更新旧任务状态为 cancelled
            let mut tasks = state.tasks.write().await;
            if let Some(old_task) = tasks.get_mut(&old_deploy.task_id) {
                old_task.status = DeployStatus::Failed;
                old_task.finished_at = Some(chrono::Utc::now());
                old_task.exit_code = Some(-2); // -2 表示被取消
            }

            // 通知 deploy-center 旧任务被取消（带重试和超时）
            if let Some(ref callback_url) = state.callback_url {
                let client = reqwest::Client::new();
                let url = format!("{}/api/deploy/logs/{}", callback_url, old_deploy.task_id);
                let old_task_id = old_deploy.task_id.clone();

                // 使用与正常完成相同的字段格式，确保 deploy-center 能正确处理
                let result = client
                    .patch(&url)
                    .timeout(std::time::Duration::from_secs(10))
                    .json(&serde_json::json!({
                        "status": "failed",
                        "exit_code": -2,  // -2 表示被新提交取消
                        "error_message": "Cancelled: superseded by newer commit"
                    }))
                    .send()
                    .await;

                match result {
                    Ok(resp) => {
                        if !resp.status().is_success() {
                            warn!(
                                task_id = %old_task_id,
                                status = %resp.status(),
                                "Failed to notify deploy-center about cancellation"
                            );
                        } else {
                            info!(
                                task_id = %old_task_id,
                                "Notified deploy-center about task cancellation"
                            );
                        }
                    }
                    Err(e) => {
                        error!(
                            task_id = %old_task_id,
                            error = %e,
                            "Failed to send cancellation notification to deploy-center"
                        );
                    }
                }
            }
        }
    }

    // 创建取消令牌
    let cancel_token = CancellationToken::new();

    // 创建日志广播通道
    let (log_tx, _) = broadcast::channel::<LogLine>(1000);

    // 创建任务
    let task = DeployTask {
        id: task_id.clone(),
        project: project.clone(),
        status: DeployStatus::Running,
        started_at: chrono::Utc::now(),
        finished_at: None,
        exit_code: None,
        stages: Vec::new(),
    };

    // 保存任务和通道
    {
        let mut tasks = state.tasks.write().await;
        tasks.insert(task_id.clone(), task);
    }
    {
        let mut channels = state.log_channels.write().await;
        channels.insert(task_id.clone(), log_tx.clone());
    }
    // 记录当前运行的部署
    {
        let mut running = state.running_deploys.write().await;
        running.insert(project.clone(), RunningDeploy {
            task_id: task_id.clone(),
            cancel_token: cancel_token.clone(),
        });
    }

    info!(
        task_id = %task_id,
        project = %project,
        commit = ?request.commit_hash,
        branch = ?request.branch,
        "Starting deployment"
    );

    // 启动部署进程
    let work_dir = config.work_dir.clone();
    let deploy_type = config.deploy_type.clone();
    let state_clone = state.clone();
    let task_id_clone = task_id.clone();
    let project_clone = project.clone();

    tokio::spawn(async move {
        match deploy_type {
            DeployType::Script { script } => {
                run_deploy(
                    state_clone,
                    task_id_clone,
                    project_clone,
                    work_dir,
                    script,
                    request.commit_hash,
                    request.branch,
                    log_tx,
                    cancel_token,
                )
                .await;
            }
            DeployType::DockerCompose {
                compose_file,
                image,
                git_repo_dir,
                service,
            } => {
                run_docker_compose_deploy(
                    state_clone,
                    task_id_clone,
                    project_clone,
                    work_dir,
                    compose_file,
                    image,
                    git_repo_dir,
                    service,
                    log_tx,
                    cancel_token,
                )
                .await;
            }
            DeployType::DockerBuild {
                dockerfile,
                build_context,
                image,
                tag,
                push_latest,
                branch,
            } => {
                run_docker_build_deploy(
                    state_clone,
                    task_id_clone,
                    project_clone,
                    work_dir,
                    dockerfile,
                    build_context,
                    image,
                    tag.or(request.commit_hash.clone()),
                    push_latest,
                    branch.or(request.branch.clone()),
                    log_tx,
                    cancel_token,
                )
                .await;
            }
            DeployType::Xiaojincut {
                task_type,
                input_path,
                output_dir,
                prompt,
                port,
            } => {
                run_xiaojincut_task(
                    state_clone,
                    task_id_clone,
                    project_clone,
                    task_type,
                    input_path,
                    output_dir,
                    prompt,
                    port,
                    log_tx,
                    cancel_token,
                )
                .await;
            }
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(TriggerResponse {
            task_id: task_id.clone(),
            project,
            status: "running".to_string(),
            stream_url: format!("/logs/{}/stream", task_id),
        }),
    ))
}

/// 执行部署脚本
async fn run_deploy(
    state: Arc<AppState>,
    task_id: String,
    project: String,
    work_dir: String,
    script: String,
    commit_hash: Option<String>,
    branch: Option<String>,
    log_tx: broadcast::Sender<LogLine>,
    cancel_token: CancellationToken,
) {
    // 创建 HTTP client 用于实时发送日志
    let http_client = reqwest::Client::new();
    let callback_url = state.callback_url.clone();
    let task_id_for_log = task_id.clone();

    // 启动心跳任务 - 每 15 秒发送一次心跳
    let heartbeat_task = {
        let http_client = http_client.clone();
        let callback_url = callback_url.clone();
        let task_id = task_id.clone();
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => break,
                    _ = interval.tick() => {
                        if let Some(ref url) = callback_url {
                            let heartbeat_url = format!("{}/api/deploy/logs/{}/heartbeat", url, task_id);
                            let _ = http_client
                                .post(&heartbeat_url)
                                .timeout(std::time::Duration::from_secs(5))
                                .send()
                                .await;
                        }
                    }
                }
            }
        })
    };

    // 部署总超时保护
    let timeout_cancel = cancel_token.clone();
    let timeout_task_id = task_id.clone();
    let timeout_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(DEPLOY_TIMEOUT_SECS)).await;
        error!(task_id = %timeout_task_id, "Script deployment timed out after {} minutes", DEPLOY_TIMEOUT_SECS / 60);
        timeout_cancel.cancel();
    });

    let send_log = |stream: &str, content: String| {
        let line = LogLine {
            timestamp: chrono::Utc::now(),
            stream: stream.to_string(),
            content,
        };
        let _ = log_tx.send(line);
    };

    // 实时发送日志到 deploy-center
    let send_log_to_center = {
        let http_client = http_client.clone();
        let callback_url = callback_url.clone();
        let task_id = task_id_for_log.clone();
        move |stream: String, content: String| {
            let http_client = http_client.clone();
            let callback_url = callback_url.clone();
            let task_id = task_id.clone();
            tokio::spawn(async move {
                if let Some(ref url) = callback_url {
                    let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id);
                    let _ = http_client
                        .post(&append_url)
                        .json(&serde_json::json!({
                            "line": content,
                            "stream": stream
                        }))
                        .send()
                        .await;
                }
            });
        }
    };

    // 辅助宏：同时发送到本地和 deploy-center
    macro_rules! log_line {
        ($stream:expr, $content:expr) => {{
            let content = $content;
            send_log($stream, content.clone());
            send_log_to_center($stream.to_string(), content);
        }};
    }

    log_line!("stdout", format!("=== Starting deployment for {} ===", project));
    log_line!("stdout", format!("Working directory: {}", work_dir));
    log_line!("stdout", format!("Script: {}", script));

    if let Some(ref hash) = commit_hash {
        log_line!("stdout", format!("Commit: {}", hash));
    }
    if let Some(ref br) = branch {
        log_line!("stdout", format!("Branch: {}", br));
    }

    // 先 git pull
    log_line!("stdout", ">>> git pull".to_string());

    let git_result = Command::new("git")
        .arg("pull")
        .current_dir(&work_dir)
        .output()
        .await;

    match git_result {
        Ok(output) => {
            if !output.stdout.is_empty() {
                log_line!("stdout", String::from_utf8_lossy(&output.stdout).to_string());
            }
            if !output.stderr.is_empty() {
                log_line!("stderr", String::from_utf8_lossy(&output.stderr).to_string());
            }
            if !output.status.success() {
                log_line!("stderr", "git pull failed, continuing anyway...".to_string());
            }
        }
        Err(e) => {
            log_line!("stderr", format!("Failed to run git pull: {}", e));
        }
    }

    // 执行构建脚本
    log_line!("stdout", format!(">>> {}", script));

    let mut child = match Command::new("bash")
        .arg("-c")
        .arg(&script)
        .current_dir(&work_dir)
        .env("COMMIT_HASH", commit_hash.as_deref().unwrap_or(""))
        .env("BRANCH", branch.as_deref().unwrap_or(""))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            error!(task_id = %task_id, error = %e, "Failed to spawn process");
            log_line!("stderr", format!("Failed to start script: {}", e));
            update_task_status(&state, &task_id, DeployStatus::Failed, Some(-1)).await;
            // Notify deploy-center about the failure
            if let Some(ref callback_url) = state.callback_url {
                let _ = notify_deploy_center(callback_url, &task_id, &project, &DeployStatus::Failed, -1).await;
            }
            return;
        }
    };

    // 流式读取 stdout
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let log_tx_stdout = log_tx.clone();
    let log_tx_stderr = log_tx.clone();

    // 为 stdout/stderr 任务准备发送到 deploy-center 的能力
    let http_client_stdout = http_client.clone();
    let http_client_stderr = http_client.clone();
    let callback_url_stdout = callback_url.clone();
    let callback_url_stderr = callback_url.clone();
    let task_id_stdout = task_id.clone();
    let task_id_stderr = task_id.clone();

    // 过滤不重要的日志行 - 保留更多有用信息
    fn should_send_to_center(line: &str) -> bool {
        let line = line.trim();
        // 过滤空行
        if line.is_empty() {
            return false;
        }
        // Docker buildx 输出 - 更宽松地保留信息
        if line.starts_with('#') && line.chars().nth(1).map(|c| c.is_ascii_digit()).unwrap_or(false) {
            // 保留: 阶段完成状态、编译信息、错误、关键阶段
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
        // 过滤重复的 resolve/sha256 行
        if line.contains("sha256:") && (line.contains("resolve") || line.contains("Pulling") || line.contains("Waiting")) {
            return false;
        }
        // 过滤 buildx 进度条
        if line.contains("[>") || line.contains("[ ") || line.contains("[=") {
            return false;
        }
        true
    }

    let stdout_task = tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = log_tx_stdout.send(LogLine {
                timestamp: chrono::Utc::now(),
                stream: "stdout".to_string(),
                content: line.clone(),
            });
            // 只发送重要日志到 deploy-center
            if let Some(ref url) = callback_url_stdout {
                if should_send_to_center(&line) {
                    let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id_stdout);
                    let _ = http_client_stdout
                        .post(&append_url)
                        .json(&serde_json::json!({
                            "line": line,
                            "stream": "stdout"
                        }))
                        .send()
                        .await;
                }
            }
        }
    });

    let stderr_task = tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = log_tx_stderr.send(LogLine {
                timestamp: chrono::Utc::now(),
                stream: "stderr".to_string(),
                content: line.clone(),
            });
            // stderr 大多是重要信息，但也过滤 buildx 进度
            if let Some(ref url) = callback_url_stderr {
                if should_send_to_center(&line) {
                    let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id_stderr);
                    let _ = http_client_stderr
                        .post(&append_url)
                        .json(&serde_json::json!({
                            "line": line,
                            "stream": "stderr"
                        }))
                        .send()
                        .await;
                }
            }
        }
    });

    // 等待进程完成，同时监听取消信号
    let (status, exit_code) = tokio::select! {
        // 取消信号（可能是被新提交取消，也可能是超时）
        _ = cancel_token.cancelled() => {
            warn!(task_id = %task_id, project = %project, "Deployment cancelled");
            log_line!("stderr", "=== Deployment CANCELLED ===".to_string());

            // 尝试杀死子进程
            if let Err(e) = child.kill().await {
                warn!(task_id = %task_id, error = %e, "Failed to kill child process");
            }

            // 清理 stdout/stderr 任务
            stdout_task.abort();
            stderr_task.abort();

            // 停止心跳和超时任务
            heartbeat_task.abort();
            timeout_task.abort();

            // 从 running_deploys 中移除（如果还在）
            {
                let mut running = state.running_deploys.write().await;
                running.remove(&project);
            }

            // 更新任务状态为失败
            update_task_status(&state, &task_id, DeployStatus::Failed, Some(-2)).await;

            // 通知 deploy-center
            if let Some(ref callback_url) = state.callback_url {
                let _ = notify_deploy_center(callback_url, &task_id, &project, &DeployStatus::Failed, -2).await;
            }

            info!(task_id = %task_id, project = %project, "Deployment cancelled, exit_code=-2");
            return;
        }

        // 正常等待进程完成
        exit_status = child.wait() => {
            // 等待 stdout/stderr 读取完成
            let _ = stdout_task.await;
            let _ = stderr_task.await;

            match exit_status {
                Ok(status) => {
                    let code = status.code().unwrap_or(-1);
                    if status.success() {
                        log_line!("stdout", format!("=== Deployment completed successfully (exit code: {}) ===", code));
                        (DeployStatus::Success, code)
                    } else {
                        log_line!("stderr", format!("=== Deployment failed (exit code: {}) ===", code));
                        (DeployStatus::Failed, code)
                    }
                }
                Err(e) => {
                    log_line!("stderr", format!("=== Process error: {} ===", e));
                    (DeployStatus::Failed, -1)
                }
            }
        }
    };

    // 停止心跳和超时任务
    heartbeat_task.abort();
    timeout_task.abort();

    // 从 running_deploys 中移除
    {
        let mut running = state.running_deploys.write().await;
        // 只有当前任务才移除（可能已经被新任务替换）
        if let Some(current) = running.get(&project) {
            if current.task_id == task_id {
                running.remove(&project);
            }
        }
    }

    // 更新任务状态
    update_task_status(&state, &task_id, status.clone(), Some(exit_code)).await;

    // 回调通知部署中心
    if let Some(ref callback_url) = state.callback_url {
        let _ = notify_deploy_center(callback_url, &task_id, &project, &status, exit_code).await;
    }

    info!(
        task_id = %task_id,
        project = %project,
        exit_code = exit_code,
        "Deployment finished"
    );
}

/// Docker Compose 部署 - 拉取镜像并重启容器
async fn run_docker_compose_deploy(
    state: Arc<AppState>,
    task_id: String,
    project: String,
    work_dir: String,
    compose_file: String,
    image: String,
    git_repo_dir: Option<String>,
    service: Option<String>,
    log_tx: broadcast::Sender<LogLine>,
    cancel_token: CancellationToken,
) {
    // 创建 HTTP client 用于实时发送日志
    let http_client = reqwest::Client::new();
    let callback_url = state.callback_url.clone();
    let task_id_for_log = task_id.clone();

    // 实时发送日志到 deploy-center
    let send_log_to_center = {
        let http_client = http_client.clone();
        let callback_url = callback_url.clone();
        let task_id = task_id_for_log.clone();
        move |stream: String, content: String| {
            let http_client = http_client.clone();
            let callback_url = callback_url.clone();
            let task_id = task_id.clone();
            tokio::spawn(async move {
                if let Some(ref url) = callback_url {
                    let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id);
                    let _ = http_client
                        .post(&append_url)
                        .json(&serde_json::json!({
                            "line": content,
                            "stream": stream
                        }))
                        .send()
                        .await;
                }
            });
        }
    };

    // 发送 stages 更新到 deploy-center
    let send_stage_update = {
        let http_client = http_client.clone();
        let callback_url = callback_url.clone();
        let task_id = task_id_for_log.clone();
        move |stages: Vec<DeployStage>| {
            let http_client = http_client.clone();
            let callback_url = callback_url.clone();
            let task_id = task_id.clone();
            tokio::spawn(async move {
                if let Some(ref url) = callback_url {
                    let stage_url = format!("{}/api/deploy/logs/{}/stages", url, task_id);
                    let _ = http_client
                        .post(&stage_url)
                        .json(&serde_json::json!({ "stages": stages }))
                        .send()
                        .await;
                }
            });
        }
    };

    // 辅助宏：同时发送到本地广播和 deploy-center
    macro_rules! log_line {
        ($stream:expr, $content:expr) => {{
            let content = $content;
            let _ = log_tx.send(LogLine {
                timestamp: chrono::Utc::now(),
                stream: $stream.to_string(),
                content: content.clone(),
            });
            send_log_to_center($stream.to_string(), content);
        }};
    }

    // 初始化阶段
    let mut stages = vec![
        DeployStage::new("git_pull", "Git Pull"),
        DeployStage::new("docker_pull", "Docker Pull"),
        DeployStage::new("compose_pull", "Compose Pull"),
        DeployStage::new("compose_up", "Compose Up"),
    ];

    log_line!("stdout", format!("=== Docker Compose Deploy for {} ===", project));
    log_line!("stdout", format!("Working directory: {}", work_dir));
    log_line!("stdout", format!("Compose file: {}", compose_file));
    log_line!("stdout", format!("Image: {}", image));
    log_line!("stdout", format!("Timestamp: {}", chrono::Utc::now().to_rfc3339()));

    let mut status = DeployStatus::Success;
    let mut exit_code = 0;

    // 构建 compose 文件的完整路径
    let compose_path = if compose_file.starts_with('/') {
        compose_file.clone()
    } else {
        format!("{}/{}", work_dir, compose_file)
    };

    // 检测 docker-compose 命令（优先使用 docker-compose，回退到 docker compose）
    let (docker_compose_cmd, docker_compose_args) = {
        let check = Command::new("which")
            .arg("docker-compose")
            .output()
            .await;
        if check.map(|o| o.status.success()).unwrap_or(false) {
            ("docker-compose", vec![])
        } else {
            ("docker", vec!["compose"])
        }
    };
    log_line!("stdout", format!("Using: {} {:?}", docker_compose_cmd, docker_compose_args));

    // Step 1: Git pull (如果配置了 git_repo_dir)
    stages[0].start();
    send_stage_update(stages.clone());
    if let Some(ref git_dir) = git_repo_dir {
        log_line!("stdout", "[1/4] Updating repository...".to_string());
        log_line!("stdout", format!(">>> git pull (in {})", git_dir));

        let git_result = Command::new("git")
            .args(["pull", "--ff-only", "origin", "master"])
            .current_dir(git_dir)
            .output()
            .await;

        match git_result {
            Ok(output) => {
                if !output.stdout.is_empty() {
                    log_line!("stdout", String::from_utf8_lossy(&output.stdout).to_string());
                }
                if !output.stderr.is_empty() {
                    log_line!("stderr", String::from_utf8_lossy(&output.stderr).to_string());
                }
                if output.status.success() {
                    stages[0].finish(true, None);
                } else {
                    stages[0].finish(false, Some("git pull failed, continuing".to_string()));
                    log_line!("stderr", "Warning: git pull failed, continuing with existing files".to_string());
                }
            }
            Err(e) => {
                stages[0].finish(false, Some(e.to_string()));
                log_line!("stderr", format!("Warning: Failed to run git pull: {}", e));
            }
        }
    } else {
        stages[0].skip(Some("not configured".to_string()));
        log_line!("stdout", "[1/4] Skipping git pull (not configured)".to_string());
    }
    send_stage_update(stages.clone());

    // 检查是否被取消
    if cancel_token.is_cancelled() {
        log_line!("stderr", "Deployment cancelled".to_string());
        status = DeployStatus::Failed;
        exit_code = -2;
    }

    // Step 2: Docker pull image
    if exit_code == 0 {
        stages[1].start();
        send_stage_update(stages.clone());
        log_line!("stdout", "[2/4] Pulling Docker image...".to_string());
        log_line!("stdout", format!(">>> docker pull {}", image));

        let pull_result = Command::new("docker")
            .args(["pull", &image])
            .output()
            .await;

        match pull_result {
            Ok(output) => {
                if !output.stdout.is_empty() {
                    log_line!("stdout", String::from_utf8_lossy(&output.stdout).to_string());
                }
                if !output.stderr.is_empty() {
                    log_line!("stderr", String::from_utf8_lossy(&output.stderr).to_string());
                }
                if output.status.success() {
                    stages[1].finish(true, None);
                } else {
                    stages[1].finish(false, Some("docker pull failed".to_string()));
                    log_line!("stderr", "Error: Failed to pull image".to_string());
                    status = DeployStatus::Failed;
                    exit_code = output.status.code().unwrap_or(-1);
                }
            }
            Err(e) => {
                stages[1].finish(false, Some(e.to_string()));
                log_line!("stderr", format!("Error: Failed to run docker pull: {}", e));
                status = DeployStatus::Failed;
                exit_code = -1;
            }
        }
        send_stage_update(stages.clone());
    }

    // 检查是否被取消
    if cancel_token.is_cancelled() && exit_code == 0 {
        log_line!("stderr", "Deployment cancelled".to_string());
        status = DeployStatus::Failed;
        exit_code = -2;
    }

    // Step 3: Docker compose pull (确保服务镜像更新)
    if exit_code == 0 {
        stages[2].start();
        send_stage_update(stages.clone());
        let service_str = service.as_deref().unwrap_or("all services");
        log_line!("stdout", format!("[3/4] Pulling {}...", service_str));

        let mut args = docker_compose_args.clone();
        args.extend(["-f", &compose_path, "pull"]);
        if let Some(ref svc) = service {
            args.push(svc);
            log_line!("stdout", format!(">>> {} -f {} pull {}", docker_compose_cmd, compose_path, svc));
        } else {
            log_line!("stdout", format!(">>> {} -f {} pull", docker_compose_cmd, compose_path));
        }

        let compose_pull = Command::new(docker_compose_cmd)
            .args(&args)
            .current_dir(&work_dir)
            .output()
            .await;

        match compose_pull {
            Ok(output) => {
                if !output.stdout.is_empty() {
                    log_line!("stdout", String::from_utf8_lossy(&output.stdout).to_string());
                }
                if !output.stderr.is_empty() {
                    // compose pull 的进度信息通常在 stderr
                    log_line!("stdout", String::from_utf8_lossy(&output.stderr).to_string());
                }
                if output.status.success() {
                    stages[2].finish(true, None);
                } else {
                    stages[2].finish(false, Some("compose pull had issues".to_string()));
                    log_line!("stderr", "Warning: compose pull had issues, continuing...".to_string());
                }
            }
            Err(e) => {
                stages[2].finish(false, Some(e.to_string()));
                log_line!("stderr", format!("Warning: Failed to run compose pull: {}", e));
            }
        }
        send_stage_update(stages.clone());
    }

    // Step 4: Docker compose up -d --force-recreate
    if exit_code == 0 {
        stages[3].start();
        send_stage_update(stages.clone());
        let service_str = service.as_deref().unwrap_or("all services");
        log_line!("stdout", format!("[4/4] Restarting {}...", service_str));

        let mut args = docker_compose_args.clone();
        args.extend(["-f", &compose_path, "up", "-d", "--force-recreate"]);
        if let Some(ref svc) = service {
            args.push(svc);
            log_line!("stdout", format!(">>> {} -f {} up -d --force-recreate {}", docker_compose_cmd, compose_path, svc));
        } else {
            log_line!("stdout", format!(">>> {} -f {} up -d --force-recreate", docker_compose_cmd, compose_path));
        }

        let compose_up = Command::new(docker_compose_cmd)
            .args(&args)
            .current_dir(&work_dir)
            .output()
            .await;

        match compose_up {
            Ok(output) => {
                if !output.stdout.is_empty() {
                    log_line!("stdout", String::from_utf8_lossy(&output.stdout).to_string());
                }
                if !output.stderr.is_empty() {
                    log_line!("stdout", String::from_utf8_lossy(&output.stderr).to_string());
                }
                if output.status.success() {
                    stages[3].finish(true, None);
                } else {
                    stages[3].finish(false, Some("compose up failed".to_string()));
                    log_line!("stderr", "Error: Failed to start services".to_string());
                    status = DeployStatus::Failed;
                    exit_code = output.status.code().unwrap_or(-1);
                }
            }
            Err(e) => {
                stages[3].finish(false, Some(e.to_string()));
                log_line!("stderr", format!("Error: Failed to run compose up: {}", e));
                status = DeployStatus::Failed;
                exit_code = -1;
            }
        }
        send_stage_update(stages.clone());
    }

    // 显示最终状态
    if exit_code == 0 {
        log_line!("stdout", "".to_string());
        log_line!("stdout", "=== Deployment Complete ===".to_string());

        // 显示容器状态
        let mut args = docker_compose_args.clone();
        args.extend(["-f", &compose_path, "ps"]);

        if let Ok(output) = Command::new(docker_compose_cmd)
            .args(&args)
            .current_dir(&work_dir)
            .output()
            .await
        {
            if !output.stdout.is_empty() {
                log_line!("stdout", String::from_utf8_lossy(&output.stdout).to_string());
            }
        }

        // 显示最后几行日志
        log_line!("stdout", "".to_string());
        log_line!("stdout", "Container logs (last 10 lines):".to_string());

        let mut args = docker_compose_args.clone();
        args.extend(["-f", &compose_path, "logs", "--tail=10"]);

        if let Ok(output) = Command::new(docker_compose_cmd)
            .args(&args)
            .current_dir(&work_dir)
            .output()
            .await
        {
            if !output.stdout.is_empty() {
                log_line!("stdout", String::from_utf8_lossy(&output.stdout).to_string());
            }
        }
    }

    // 打印 Stage Summary
    log_line!("stdout", "".to_string());
    log_line!("stdout", "=== Stage Summary ===".to_string());
    for stage in &stages {
        let duration = stage.duration_ms.map(|d| format!("{}ms", d)).unwrap_or_else(|| "-".to_string());
        let status_icon = match stage.status {
            StageStatus::Success => "✓",
            StageStatus::Failed => "✗",
            StageStatus::Skipped => "⊘",
            StageStatus::Running => "⟳",
            StageStatus::Pending => "○",
        };
        log_line!("stdout", format!("{} {} ({})", status_icon, stage.display_name, duration));
    }

    // 从 running_deploys 中移除
    {
        let mut running = state.running_deploys.write().await;
        if let Some(current) = running.get(&project) {
            if current.task_id == task_id {
                running.remove(&project);
            }
        }
    }

    // 更新任务状态
    update_task_status(&state, &task_id, status.clone(), Some(exit_code)).await;

    // 回调通知部署中心（带 stages）
    if let Some(ref callback_url) = state.callback_url {
        let _ = notify_deploy_center_with_stages(callback_url, &task_id, &project, &status, exit_code, &stages).await;
    }

    info!(
        task_id = %task_id,
        project = %project,
        exit_code = exit_code,
        "Docker Compose deployment finished"
    );
}

/// 清理僵尸 docker 进程和卡住的 docker build 进程
/// 在每次开始新的 docker build 之前调用，避免僵尸进程影响新的构建
async fn cleanup_zombie_docker_processes() -> (usize, usize) {
    let mut zombies_killed = 0;
    let mut hung_builds_killed = 0;

    // 1. 检查僵尸进程 (状态为 Z 或包含 <defunct>)
    if let Ok(output) = Command::new("ps")
        .args(["aux"])
        .output()
        .await
    {
        let ps_output = String::from_utf8_lossy(&output.stdout);
        for line in ps_output.lines() {
            // 检查是否是 docker 相关的僵尸进程
            if (line.contains("docker") || line.contains("buildkit"))
                && (line.contains("<defunct>") || line.contains(" Z ") || line.contains(" Z+ "))
            {
                // 提取 PID (第二列)
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(pid) = parts[1].parse::<i32>() {
                        info!("Found zombie docker process: PID={}, killing...", pid);
                        if let Ok(_) = Command::new("kill")
                            .args(["-9", &pid.to_string()])
                            .output()
                            .await
                        {
                            zombies_killed += 1;
                        }
                    }
                }
            }
        }
    }

    // 2. 检查长时间运行的 docker build 进程 (超过30分钟)
    if let Ok(output) = Command::new("ps")
        .args(["-eo", "pid,etimes,args"])
        .output()
        .await
    {
        let ps_output = String::from_utf8_lossy(&output.stdout);
        for line in ps_output.lines().skip(1) {
            // 跳过标题行
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let pid = parts[0];
                let elapsed_secs: i64 = parts[1].parse().unwrap_or(0);
                let cmd = parts[2..].join(" ");

                // 超过30分钟(1800秒)的 docker build 进程
                if elapsed_secs > 1800
                    && (cmd.contains("docker build") || cmd.contains("docker-buildx"))
                {
                    info!(
                        "Found hung docker build process: PID={}, running for {}s, killing...",
                        pid, elapsed_secs
                    );
                    if let Ok(_) = Command::new("kill")
                        .args(["-9", pid])
                        .output()
                        .await
                    {
                        hung_builds_killed += 1;
                    }
                }
            }
        }
    }

    // 3. 清理孤立的 buildkit 进程
    if let Ok(output) = Command::new("pgrep")
        .args(["-f", "buildkit"])
        .output()
        .await
    {
        if output.status.success() {
            let pids = String::from_utf8_lossy(&output.stdout);
            for pid in pids.lines() {
                if !pid.trim().is_empty() {
                    // 检查这个进程的父进程是否是 init (PID 1)，表示是孤儿进程
                    if let Ok(ppid_output) = Command::new("ps")
                        .args(["-o", "ppid=", "-p", pid.trim()])
                        .output()
                        .await
                    {
                        let ppid = String::from_utf8_lossy(&ppid_output.stdout)
                            .trim()
                            .to_string();
                        if ppid == "1" {
                            info!("Found orphan buildkit process: PID={}, killing...", pid.trim());
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

/// 部署超时时间（30 分钟）
const DEPLOY_TIMEOUT_SECS: u64 = 30 * 60;
/// 心跳间隔（15 秒）
const HEARTBEAT_INTERVAL_SECS: u64 = 15;

/// Docker Build 部署 - 拉取代码、构建镜像、推送到 registry
#[allow(clippy::too_many_arguments)]
async fn run_docker_build_deploy(
    state: Arc<AppState>,
    task_id: String,
    project: String,
    work_dir: String,
    dockerfile: Option<String>,
    build_context: Option<String>,
    image: String,
    tag: Option<String>,
    push_latest: bool,
    branch: Option<String>,
    log_tx: broadcast::Sender<LogLine>,
    cancel_token: CancellationToken,
) {
    // 创建 HTTP client
    let http_client = reqwest::Client::new();
    let callback_url = state.callback_url.clone();
    let task_id_for_log = task_id.clone();

    // 启动心跳任务 - 每 15 秒发送一次心跳，确保 deploy center 知道我们还活着
    let heartbeat_task = {
        let http_client = http_client.clone();
        let callback_url = callback_url.clone();
        let task_id = task_id.clone();
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        info!(task_id = %task_id, "Heartbeat task cancelled");
                        break;
                    }
                    _ = interval.tick() => {
                        if let Some(ref url) = callback_url {
                            let heartbeat_url = format!("{}/api/deploy/logs/{}/heartbeat", url, task_id);
                            match http_client
                                .post(&heartbeat_url)
                                .timeout(std::time::Duration::from_secs(5))
                                .send()
                                .await
                            {
                                Ok(resp) if resp.status().is_success() => {
                                    // 心跳成功，不记录以减少日志噪音
                                }
                                Ok(resp) => {
                                    warn!(task_id = %task_id, status = %resp.status(), "Heartbeat returned non-success");
                                }
                                Err(e) => {
                                    warn!(task_id = %task_id, error = %e, "Heartbeat failed");
                                }
                            }
                        }
                    }
                }
            }
        })
    };

    // 部署总超时保护
    let timeout_cancel = cancel_token.clone();
    let timeout_task_id = task_id.clone();
    let timeout_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(DEPLOY_TIMEOUT_SECS)).await;
        error!(task_id = %timeout_task_id, "Deployment timed out after {} minutes", DEPLOY_TIMEOUT_SECS / 60);
        timeout_cancel.cancel();
    });

    // 实时发送日志到 deploy-center
    let send_log_to_center = {
        let http_client = http_client.clone();
        let callback_url = callback_url.clone();
        let task_id = task_id_for_log.clone();
        move |stream: String, content: String| {
            let http_client = http_client.clone();
            let callback_url = callback_url.clone();
            let task_id = task_id.clone();
            tokio::spawn(async move {
                if let Some(ref url) = callback_url {
                    let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id);
                    let _ = http_client
                        .post(&append_url)
                        .json(&serde_json::json!({
                            "line": content,
                            "stream": stream
                        }))
                        .send()
                        .await;
                }
            });
        }
    };

    // 发送阶段更新到 deploy-center
    let send_stage_update = {
        let http_client = http_client.clone();
        let callback_url = callback_url.clone();
        let task_id = task_id.clone();
        move |stages: Vec<DeployStage>| {
            let http_client = http_client.clone();
            let callback_url = callback_url.clone();
            let task_id = task_id.clone();
            tokio::spawn(async move {
                if let Some(ref url) = callback_url {
                    let stage_url = format!("{}/api/deploy/logs/{}/stages", url, task_id);
                    let _ = http_client
                        .post(&stage_url)
                        .json(&serde_json::json!({ "stages": stages }))
                        .send()
                        .await;
                }
            });
        }
    };

    // 辅助宏
    macro_rules! log_line {
        ($stream:expr, $content:expr) => {{
            let content = $content;
            let _ = log_tx.send(LogLine {
                timestamp: chrono::Utc::now(),
                stream: $stream.to_string(),
                content: content.clone(),
            });
            send_log_to_center($stream.to_string(), content);
        }};
    }

    // 初始化阶段
    let mut stages = vec![
        DeployStage::new("git_pull", "Git Pull"),
        DeployStage::new("docker_build", "Docker Build"),
        DeployStage::new("docker_push", "Docker Push"),
    ];
    if push_latest {
        stages.push(DeployStage::new("docker_push_latest", "Push Latest Tag"));
    }

    let dockerfile_path = dockerfile.as_deref().unwrap_or("Dockerfile");
    let context_path = build_context.as_deref().unwrap_or(".");
    let branch_name = branch.as_deref().unwrap_or("master");

    log_line!("stdout", format!("=== Docker Build Deploy for {} ===", project));
    log_line!("stdout", format!("Working directory: {}", work_dir));
    log_line!("stdout", format!("Dockerfile: {}", dockerfile_path));
    log_line!("stdout", format!("Build context: {}", context_path));
    log_line!("stdout", format!("Target image: {}", image));
    log_line!("stdout", format!("Branch: {}", branch_name));
    log_line!("stdout", format!("Timestamp: {}", chrono::Utc::now().to_rfc3339()));

    let mut status = DeployStatus::Success;
    let mut exit_code = 0;
    let mut commit_hash = tag.clone().unwrap_or_else(|| "latest".to_string());

    // ============================================
    // Stage 1: Git Pull
    // ============================================
    stages[0].start();
    send_stage_update(stages.clone());
    log_line!("stdout", "[1/4] Pulling latest code...".to_string());
    log_line!("stdout", format!(">>> git pull --ff-only origin {}", branch_name));

    let git_result = Command::new("git")
        .args(["pull", "--ff-only", "origin", branch_name])
        .current_dir(&work_dir)
        .output()
        .await;

    match git_result {
        Ok(output) => {
            if !output.stdout.is_empty() {
                log_line!("stdout", String::from_utf8_lossy(&output.stdout).to_string());
            }
            if !output.stderr.is_empty() {
                log_line!("stderr", String::from_utf8_lossy(&output.stderr).to_string());
            }
            if output.status.success() {
                stages[0].finish(true, None);
            } else {
                stages[0].finish(false, Some("git pull failed".to_string()));
                status = DeployStatus::Failed;
                exit_code = output.status.code().unwrap_or(-1);
            }
        }
        Err(e) => {
            log_line!("stderr", format!("Error: Failed to run git pull: {}", e));
            stages[0].finish(false, Some(e.to_string()));
            status = DeployStatus::Failed;
            exit_code = -1;
        }
    }
    send_stage_update(stages.clone());

    // 获取当前 commit hash 作为 tag
    if exit_code == 0 && tag.is_none() {
        let hash_result = Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(&work_dir)
            .output()
            .await;

        if let Ok(output) = hash_result {
            if output.status.success() {
                commit_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
                log_line!("stdout", format!("Using commit hash as tag: {}", commit_hash));
            }
        }
    }

    // 检查取消
    if cancel_token.is_cancelled() {
        log_line!("stderr", "Deployment cancelled".to_string());
        status = DeployStatus::Failed;
        exit_code = -2;
    }

    // ============================================
    // Stage 2: Docker Build
    // ============================================
    if exit_code == 0 {
        // 清理僵尸进程和卡住的 docker build 进程
        log_line!("stdout", "Checking for zombie docker processes...".to_string());
        let (zombies, hung) = cleanup_zombie_docker_processes().await;
        if zombies > 0 || hung > 0 {
            log_line!("stdout", format!("Cleaned up {} zombie and {} hung docker processes", zombies, hung));
        }

        stages[1].start();
        send_stage_update(stages.clone());

        let full_image = format!("{}:{}", image, commit_hash);
        log_line!("stdout", "[2/4] Building Docker image...".to_string());
        log_line!("stdout", format!(">>> docker build -t {} -f {} {}", full_image, dockerfile_path, context_path));

        let build_result = Command::new("docker")
            .args([
                "build",
                "--progress=plain",  // 使用 plain 格式以便流式输出日志
                "-t", &full_image,
                "-f", dockerfile_path,
                context_path,
            ])
            .current_dir(&work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        match build_result {
            Ok(mut child) => {
                // 流式读取构建输出 - BuildKit 输出到 stderr
                let stderr = child.stderr.take();
                let stdout = child.stdout.take();

                // 并发读取 stdout 和 stderr
                let stderr_task = tokio::spawn({
                    let log_tx = log_tx.clone();
                    let http_client = http_client.clone();
                    let callback_url = callback_url.clone();
                    let task_id = task_id.clone();
                    async move {
                        if let Some(stderr) = stderr {
                            let reader = BufReader::new(stderr);
                            let mut lines = reader.lines();
                            while let Ok(Some(line)) = lines.next_line().await {
                                // 发送到本地日志
                                let _ = log_tx.send(LogLine {
                                    timestamp: chrono::Utc::now(),
                                    stream: "stderr".to_string(),
                                    content: line.clone(),
                                });
                                // 异步发送到 deploy-center，不阻塞管道读取（避免反压导致构建变慢）
                                if let Some(ref url) = callback_url {
                                    let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id);
                                    let client = http_client.clone();
                                    let body = serde_json::json!({
                                        "line": line,
                                        "stream": "stderr"
                                    });
                                    tokio::spawn(async move {
                                        let _ = client
                                            .post(&append_url)
                                            .json(&body)
                                            .timeout(std::time::Duration::from_secs(3))
                                            .send()
                                            .await;
                                    });
                                }
                            }
                        }
                    }
                });

                let stdout_task = tokio::spawn({
                    let log_tx = log_tx.clone();
                    let http_client = http_client.clone();
                    let callback_url = callback_url.clone();
                    let task_id = task_id.clone();
                    async move {
                        if let Some(stdout) = stdout {
                            let reader = BufReader::new(stdout);
                            let mut lines = reader.lines();
                            while let Ok(Some(line)) = lines.next_line().await {
                                let _ = log_tx.send(LogLine {
                                    timestamp: chrono::Utc::now(),
                                    stream: "stdout".to_string(),
                                    content: line.clone(),
                                });
                                // 异步发送到 deploy-center，不阻塞管道读取
                                if let Some(ref url) = callback_url {
                                    let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id);
                                    let client = http_client.clone();
                                    let body = serde_json::json!({
                                        "line": line,
                                        "stream": "stdout"
                                    });
                                    tokio::spawn(async move {
                                        let _ = client
                                            .post(&append_url)
                                            .json(&body)
                                            .timeout(std::time::Duration::from_secs(3))
                                            .send()
                                            .await;
                                    });
                                }
                            }
                        }
                    }
                });

                // 等待构建完成，同时监听取消信号
                let build_result = tokio::select! {
                    // 取消信号
                    _ = cancel_token.cancelled() => {
                        warn!(task_id = %task_id, project = %project, "Docker build cancelled by newer commit");
                        log_line!("stderr", "=== Docker build CANCELLED: superseded by newer commit ===".to_string());

                        // 杀死构建进程
                        if let Err(e) = child.kill().await {
                            warn!(task_id = %task_id, error = %e, "Failed to kill docker build process");
                        }

                        // 中止输出任务
                        stderr_task.abort();
                        stdout_task.abort();

                        // 标记所有未完成的阶段为跳过
                        for stage in stages.iter_mut() {
                            if matches!(stage.status, StageStatus::Pending | StageStatus::Running) {
                                stage.status = StageStatus::Skipped;
                            }
                        }

                        // 更新状态并通知 deploy-center
                        status = DeployStatus::Failed;
                        exit_code = -2;

                        log_line!("stdout", "=== Docker Build Deploy cancelled ===".to_string());

                        // 停止心跳和超时任务
                        heartbeat_task.abort();
                        timeout_task.abort();

                        update_task_status_with_stages(&state, &task_id, status.clone(), Some(exit_code), stages.clone()).await;

                        if let Some(ref callback_url) = state.callback_url {
                            let _ = notify_deploy_center_with_stages(callback_url, &task_id, &project, &status, exit_code, &stages).await;
                        }

                        info!(
                            task_id = %task_id,
                            project = %project,
                            exit_code = exit_code,
                            "Docker Build deployment cancelled"
                        );
                        return;  // 提前退出函数
                    }

                    // 正常等待构建完成
                    result = child.wait() => {
                        // 等待输出读取完成
                        let _ = tokio::join!(stderr_task, stdout_task);
                        result
                    }
                };

                match build_result {
                    Ok(exit_status) => {
                        if exit_status.success() {
                            stages[1].finish(true, None);
                            log_line!("stdout", format!("✓ Image built: {}", full_image));
                        } else {
                            stages[1].finish(false, Some("docker build failed".to_string()));
                            status = DeployStatus::Failed;
                            exit_code = exit_status.code().unwrap_or(-1);
                        }
                    }
                    Err(e) => {
                        stages[1].finish(false, Some(e.to_string()));
                        log_line!("stderr", format!("Error waiting for build: {}", e));
                        status = DeployStatus::Failed;
                        exit_code = -1;
                    }
                }
            }
            Err(e) => {
                stages[1].finish(false, Some(e.to_string()));
                log_line!("stderr", format!("Error: Failed to start docker build: {}", e));
                status = DeployStatus::Failed;
                exit_code = -1;
            }
        }
        send_stage_update(stages.clone());
    }

    // ============================================
    // Stage 3: Docker Push
    // ============================================
    // 检查取消
    if exit_code == 0 && cancel_token.is_cancelled() {
        log_line!("stderr", "=== Deployment cancelled before push ===".to_string());
        status = DeployStatus::Failed;
        exit_code = -2;
    }

    if exit_code == 0 {
        stages[2].start();
        send_stage_update(stages.clone());

        let full_image = format!("{}:{}", image, commit_hash);
        log_line!("stdout", "[3/4] Pushing image to registry...".to_string());
        log_line!("stdout", format!(">>> docker push {}", full_image));

        let push_result = Command::new("docker")
            .args(["push", &full_image])
            .output()
            .await;

        match push_result {
            Ok(output) => {
                if !output.stdout.is_empty() {
                    log_line!("stdout", String::from_utf8_lossy(&output.stdout).to_string());
                }
                if !output.stderr.is_empty() {
                    log_line!("stderr", String::from_utf8_lossy(&output.stderr).to_string());
                }
                if output.status.success() {
                    stages[2].finish(true, None);
                    log_line!("stdout", format!("✓ Pushed: {}", full_image));
                } else {
                    stages[2].finish(false, Some("docker push failed".to_string()));
                    status = DeployStatus::Failed;
                    exit_code = output.status.code().unwrap_or(-1);
                }
            }
            Err(e) => {
                stages[2].finish(false, Some(e.to_string()));
                log_line!("stderr", format!("Error: Failed to push image: {}", e));
                status = DeployStatus::Failed;
                exit_code = -1;
            }
        }
        send_stage_update(stages.clone());
    }

    // ============================================
    // Stage 4: Push Latest Tag (optional)
    // ============================================
    // 检查取消
    if exit_code == 0 && cancel_token.is_cancelled() {
        log_line!("stderr", "=== Deployment cancelled before push latest ===".to_string());
        status = DeployStatus::Failed;
        exit_code = -2;
    }

    if exit_code == 0 && push_latest && stages.len() > 3 {
        stages[3].start();
        send_stage_update(stages.clone());

        let full_image = format!("{}:{}", image, commit_hash);
        let latest_image = format!("{}:latest", image);
        log_line!("stdout", "[4/4] Tagging and pushing latest...".to_string());

        // Tag as latest
        log_line!("stdout", format!(">>> docker tag {} {}", full_image, latest_image));
        let tag_result = Command::new("docker")
            .args(["tag", &full_image, &latest_image])
            .output()
            .await;

        if let Ok(output) = tag_result {
            if !output.status.success() {
                log_line!("stderr", "Warning: Failed to tag as latest".to_string());
            }
        }

        // Push latest
        log_line!("stdout", format!(">>> docker push {}", latest_image));
        let push_latest_result = Command::new("docker")
            .args(["push", &latest_image])
            .output()
            .await;

        match push_latest_result {
            Ok(output) => {
                if !output.stdout.is_empty() {
                    log_line!("stdout", String::from_utf8_lossy(&output.stdout).to_string());
                }
                if output.status.success() {
                    stages[3].finish(true, None);
                    log_line!("stdout", format!("✓ Pushed: {}", latest_image));
                } else {
                    stages[3].finish(false, Some("push latest failed".to_string()));
                    // Don't fail the whole deploy for latest tag
                    log_line!("stderr", "Warning: Failed to push latest tag".to_string());
                }
            }
            Err(e) => {
                stages[3].finish(false, Some(e.to_string()));
                log_line!("stderr", format!("Warning: Failed to push latest: {}", e));
            }
        }
        send_stage_update(stages.clone());
    }

    // 完成
    log_line!("stdout", "=== Docker Build Deploy finished ===".to_string());
    log_line!("stdout", format!("Status: {:?}", status));
    log_line!("stdout", format!("Exit code: {}", exit_code));

    // 打印阶段摘要
    log_line!("stdout", "\n=== Stage Summary ===".to_string());
    for stage in &stages {
        let duration = stage.duration_ms.map(|d| format!("{}ms", d)).unwrap_or_else(|| "-".to_string());
        let status_icon = match stage.status {
            StageStatus::Success => "✓",
            StageStatus::Failed => "✗",
            StageStatus::Skipped => "⊘",
            StageStatus::Running => "⟳",
            StageStatus::Pending => "○",
        };
        log_line!("stdout", format!("{} {} ({})", status_icon, stage.display_name, duration));
    }

    // 停止心跳和超时任务
    heartbeat_task.abort();
    timeout_task.abort();

    // 更新任务状态（包含 stages）
    update_task_status_with_stages(&state, &task_id, status.clone(), Some(exit_code), stages.clone()).await;

    // 回调通知部署中心
    if let Some(ref callback_url) = state.callback_url {
        let _ = notify_deploy_center_with_stages(callback_url, &task_id, &project, &status, exit_code, &stages).await;
    }

    info!(
        task_id = %task_id,
        project = %project,
        exit_code = exit_code,
        "Docker Build deployment finished"
    );
}

/// 更新任务状态（包含阶段信息）
async fn update_task_status_with_stages(
    state: &Arc<AppState>,
    task_id: &str,
    status: DeployStatus,
    exit_code: Option<i32>,
    stages: Vec<DeployStage>,
) {
    let completed_task = {
        let mut tasks = state.tasks.write().await;
        if let Some(task) = tasks.get_mut(task_id) {
            task.status = status.clone();
            task.finished_at = Some(chrono::Utc::now());
            task.exit_code = exit_code;
            task.stages = stages;
            Some(task.clone())
        } else {
            None
        }
    };

    // 保存到历史记录
    if let Some(task) = completed_task {
        let mut history = state.task_history.write().await;
        if history.len() >= MAX_TASK_HISTORY {
            history.pop_front();
        }
        history.push_back(task);
    }
}

/// 回调通知部署中心（包含阶段信息）
/// 带超时和重试机制，确保状态更新不会丢失
async fn notify_deploy_center_with_stages(
    callback_url: &str,
    task_id: &str,
    project: &str,
    status: &DeployStatus,
    exit_code: i32,
    stages: &[DeployStage],
) -> Result<(), reqwest::Error> {
    let client = reqwest::Client::new();
    let status_str = match status {
        DeployStatus::Success => "success",
        DeployStatus::Failed => "failed",
        DeployStatus::Running => "running",
    };

    let url = format!("{}/api/deploy/logs/{}", callback_url, task_id);
    let body = serde_json::json!({
        "status": status_str,
        "exit_code": exit_code,
        "stages": stages
    });

    // 最多重试3次，每次超时10秒
    let mut last_error = None;
    for attempt in 1..=3 {
        match client
            .patch(&url)
            .timeout(std::time::Duration::from_secs(10))
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status().is_success() {
                    info!(
                        task_id = %task_id,
                        project = %project,
                        status = %status_str,
                        attempt = attempt,
                        "Notified deploy center with stages"
                    );
                    return Ok(());
                } else {
                    warn!(
                        task_id = %task_id,
                        project = %project,
                        status = %resp.status(),
                        attempt = attempt,
                        "Deploy center returned non-success status"
                    );
                }
            }
            Err(e) => {
                warn!(
                    task_id = %task_id,
                    project = %project,
                    error = %e,
                    attempt = attempt,
                    "Failed to notify deploy center, will retry"
                );
                last_error = Some(e);
            }
        }

        // 重试前等待
        if attempt < 3 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    // 所有重试都失败了
    error!(
        task_id = %task_id,
        project = %project,
        "Failed to notify deploy center after 3 attempts"
    );

    if let Some(e) = last_error {
        Err(e)
    } else {
        Ok(()) // 响应不是成功但没有网络错误
    }
}

/// 更新任务状态
async fn update_task_status(
    state: &Arc<AppState>,
    task_id: &str,
    status: DeployStatus,
    exit_code: Option<i32>,
) {
    let completed_task = {
        let mut tasks = state.tasks.write().await;
        if let Some(task) = tasks.get_mut(task_id) {
            task.status = status.clone();
            task.finished_at = Some(chrono::Utc::now());
            task.exit_code = exit_code;

            // 如果任务已完成（成功或失败），返回副本以添加到历史记录
            match status {
                DeployStatus::Success | DeployStatus::Failed => Some(task.clone()),
                DeployStatus::Running => None,
            }
        } else {
            None
        }
    };

    // 添加已完成的任务到历史记录
    if let Some(task) = completed_task {
        state.add_to_history(task).await;
    }
}

/// 通知部署中心
/// 带超时和重试机制，确保状态更新不会丢失
async fn notify_deploy_center(
    callback_url: &str,
    task_id: &str,
    project: &str,
    status: &DeployStatus,
    exit_code: i32,
) -> Result<(), reqwest::Error> {
    let client = reqwest::Client::new();
    let status_str = match status {
        DeployStatus::Success => "success",
        DeployStatus::Failed => "failed",
        DeployStatus::Running => "running",
    };

    let url = format!("{}/api/deploy/logs/{}", callback_url, task_id);
    let body = serde_json::json!({
        "status": status_str,
        "exit_code": exit_code,
    });

    // 最多重试3次，每次超时10秒
    let mut last_error = None;
    for attempt in 1..=3 {
        match client
            .patch(&url)
            .timeout(std::time::Duration::from_secs(10))
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status().is_success() {
                    info!(
                        task_id = %task_id,
                        project = %project,
                        status = %status_str,
                        attempt = attempt,
                        "Notified deploy center"
                    );
                    return Ok(());
                } else {
                    warn!(
                        task_id = %task_id,
                        project = %project,
                        status = %resp.status(),
                        attempt = attempt,
                        "Deploy center returned non-success status"
                    );
                }
            }
            Err(e) => {
                warn!(
                    task_id = %task_id,
                    project = %project,
                    error = %e,
                    attempt = attempt,
                    "Failed to notify deploy center, will retry"
                );
                last_error = Some(e);
            }
        }

        // 重试前等待
        if attempt < 3 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    // 所有重试都失败了
    error!(
        task_id = %task_id,
        project = %project,
        "Failed to notify deploy center after 3 attempts"
    );

    if let Some(e) = last_error {
        Err(e)
    } else {
        Ok(()) // 响应不是成功但没有网络错误
    }
}

/// 从部署中心获取项目配置
async fn fetch_project_config_from_center(
    deploy_center_url: &str,
    project_slug: &str,
) -> Option<ProjectConfig> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let url = format!("{}/api/deploy/agent-config/{}", deploy_center_url, project_slug);

    match client.get(&url).send().await {
        Ok(response) => {
            if response.status().is_success() {
                match response.json::<RemoteAgentConfigResponse>().await {
                    Ok(remote_config) => {
                        info!(
                            project = %project_slug,
                            deploy_type = %remote_config.config.deploy_type,
                            "Fetched project config from deploy center"
                        );
                        remote_config.config.to_project_config()
                    }
                    Err(e) => {
                        warn!(project = %project_slug, error = %e, "Failed to parse remote config");
                        None
                    }
                }
            } else {
                info!(
                    project = %project_slug,
                    status = %response.status(),
                    "No config found in deploy center, will use local config"
                );
                None
            }
        }
        Err(e) => {
            warn!(project = %project_slug, error = %e, "Failed to fetch config from deploy center");
            None
        }
    }
}

/// 获取任务状态
async fn get_task_status(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let tasks = state.tasks.read().await;
    let task = tasks.get(&task_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".to_string(),
                message: format!("Task '{}' not found", task_id),
            }),
        )
    })?;

    Ok(Json(task.clone()))
}

/// 执行 Xiaojincut 本地视频处理任务
async fn run_xiaojincut_task(
    state: Arc<AppState>,
    task_id: String,
    project: String,
    task_type: String,
    input_path: Option<String>,
    output_dir: Option<String>,
    prompt: Option<String>,
    port: Option<u16>,
    log_tx: broadcast::Sender<LogLine>,
    cancel_token: CancellationToken,
) {
    let port = port.unwrap_or(19527);
    let base_url = format!("http://localhost:{}/api", port);
    let http_client = reqwest::Client::new();
    let callback_url = state.callback_url.clone();
    let task_id_for_log = task_id.clone();

    // 辅助宏：发送日志
    macro_rules! log_line {
        ($stream:expr, $content:expr) => {{
            let content = $content;
            let _ = log_tx.send(LogLine {
                timestamp: chrono::Utc::now(),
                stream: $stream.to_string(),
                content: content.clone(),
            });
            // 发送到 deploy-center
            if let Some(ref url) = callback_url {
                let http_client = http_client.clone();
                let url = url.clone();
                let task_id = task_id_for_log.clone();
                tokio::spawn(async move {
                    let append_url = format!("{}/api/deploy/logs/{}/append", url, task_id);
                    let _ = http_client
                        .post(&append_url)
                        .json(&serde_json::json!({
                            "line": content,
                            "stream": $stream
                        }))
                        .send()
                        .await;
                });
            }
        }};
    }

    log_line!("stdout", format!("═══════════════════════════════════════════════════════════"));
    log_line!("stdout", format!("Xiaojincut Task: {}", task_type));
    log_line!("stdout", format!("═══════════════════════════════════════════════════════════"));
    log_line!("stdout", format!("Target: http://localhost:{}", port));
    if let Some(ref path) = input_path {
        log_line!("stdout", format!("Input: {}", path));
    }
    if let Some(ref dir) = output_dir {
        log_line!("stdout", format!("Output Dir: {}", dir));
    }
    if let Some(ref p) = prompt {
        log_line!("stdout", format!("Prompt: {}", p));
    }
    log_line!("stdout", "───────────────────────────────────────────────────────────".to_string());

    // 检查 xiaojincut 服务是否运行
    log_line!("stdout", ">>> Checking xiaojincut service health...".to_string());

    let health_url = format!("{}/health", base_url);
    let health_result = tokio::select! {
        _ = cancel_token.cancelled() => {
            log_line!("stderr", "Task cancelled".to_string());
            update_task_status(&state, &task_id, DeployStatus::Failed, Some(-1)).await;
            return;
        }
        result = http_client.get(&health_url).timeout(std::time::Duration::from_secs(5)).send() => result
    };

    match health_result {
        Ok(resp) if resp.status().is_success() => {
            log_line!("stdout", "✅ xiaojincut service is running".to_string());
        }
        Ok(resp) => {
            log_line!("stderr", format!("❌ xiaojincut health check failed: HTTP {}", resp.status()));
            update_task_status(&state, &task_id, DeployStatus::Failed, Some(1)).await;
            if let Some(ref url) = state.callback_url {
                let _ = notify_deploy_center(url, &task_id, &project, &DeployStatus::Failed, 1).await;
            }
            return;
        }
        Err(e) => {
            log_line!("stderr", format!("❌ xiaojincut service not reachable: {}", e));
            log_line!("stderr", "Tip: Start xiaojincut with: open /Applications/xiaojincut.app".to_string());
            update_task_status(&state, &task_id, DeployStatus::Failed, Some(1)).await;
            if let Some(ref url) = state.callback_url {
                let _ = notify_deploy_center(url, &task_id, &project, &DeployStatus::Failed, 1).await;
            }
            return;
        }
    }

    // 根据任务类型执行不同操作
    let result = match task_type.as_str() {
        "import" => {
            if let Some(path) = input_path {
                log_line!("stdout", format!(">>> Importing media: {}", path));
                let import_url = format!("{}/import", base_url);
                http_client
                    .post(&import_url)
                    .json(&serde_json::json!({ "path": path }))
                    .timeout(std::time::Duration::from_secs(60))
                    .send()
                    .await
            } else {
                log_line!("stderr", "❌ Missing input_path for import task".to_string());
                update_task_status(&state, &task_id, DeployStatus::Failed, Some(1)).await;
                if let Some(ref url) = state.callback_url {
                    let _ = notify_deploy_center(url, &task_id, &project, &DeployStatus::Failed, 1).await;
                }
                return;
            }
        }
        "export" => {
            log_line!("stdout", ">>> Exporting timeline...".to_string());
            let export_url = format!("{}/export", base_url);
            let mut payload = serde_json::json!({});
            if let Some(dir) = output_dir {
                payload["output_dir"] = serde_json::Value::String(dir);
            }
            http_client
                .post(&export_url)
                .json(&payload)
                .timeout(std::time::Duration::from_secs(300))
                .send()
                .await
        }
        "status" => {
            log_line!("stdout", ">>> Getting status...".to_string());
            let status_url = format!("{}/status", base_url);
            http_client
                .get(&status_url)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
        }
        "timeline" => {
            log_line!("stdout", ">>> Getting timeline...".to_string());
            let timeline_url = format!("{}/timeline", base_url);
            http_client
                .get(&timeline_url)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
        }
        _ => {
            log_line!("stderr", format!("❌ Unknown task type: {}", task_type));
            update_task_status(&state, &task_id, DeployStatus::Failed, Some(1)).await;
            if let Some(ref url) = state.callback_url {
                let _ = notify_deploy_center(url, &task_id, &project, &DeployStatus::Failed, 1).await;
            }
            return;
        }
    };

    // 处理响应
    match result {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_else(|_| "".to_string());

            if status.is_success() {
                log_line!("stdout", "───────────────────────────────────────────────────────────".to_string());
                log_line!("stdout", format!("✅ Task completed successfully"));

                // 尝试格式化 JSON 输出
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                    let pretty = serde_json::to_string_pretty(&json).unwrap_or(body.clone());
                    for line in pretty.lines() {
                        log_line!("stdout", line.to_string());
                    }
                } else if !body.is_empty() {
                    log_line!("stdout", body);
                }

                log_line!("stdout", "═══════════════════════════════════════════════════════════".to_string());
                update_task_status(&state, &task_id, DeployStatus::Success, Some(0)).await;
                if let Some(ref url) = state.callback_url {
                    let _ = notify_deploy_center(url, &task_id, &project, &DeployStatus::Success, 0).await;
                }
            } else {
                log_line!("stderr", format!("❌ Task failed: HTTP {}", status));
                if !body.is_empty() {
                    log_line!("stderr", body);
                }
                update_task_status(&state, &task_id, DeployStatus::Failed, Some(1)).await;
                if let Some(ref url) = state.callback_url {
                    let _ = notify_deploy_center(url, &task_id, &project, &DeployStatus::Failed, 1).await;
                }
            }
        }
        Err(e) => {
            log_line!("stderr", format!("❌ Request failed: {}", e));
            update_task_status(&state, &task_id, DeployStatus::Failed, Some(1)).await;
            if let Some(ref url) = state.callback_url {
                let _ = notify_deploy_center(url, &task_id, &project, &DeployStatus::Failed, 1).await;
            }
        }
    }
}

/// 流式获取日志
async fn stream_logs(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, Json<ErrorResponse>)> {
    // 获取日志通道
    let channels = state.log_channels.read().await;
    let tx = channels.get(&task_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".to_string(),
                message: format!("Task '{}' not found or already completed", task_id),
            }),
        )
    })?;

    let mut rx = tx.subscribe();
    drop(channels);

    let state_clone = state.clone();
    let task_id_clone = task_id.clone();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(log_line) => {
                    let json = serde_json::to_string(&log_line).unwrap_or_default();
                    yield Ok(Event::default().data(json));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(task_id = %task_id_clone, lagged = n, "Log subscriber lagged");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // 发送结束事件
                    let tasks = state_clone.tasks.read().await;
                    if let Some(task) = tasks.get(&task_id_clone) {
                        let status = match task.status {
                            DeployStatus::Success => "success",
                            DeployStatus::Failed => "failed",
                            DeployStatus::Running => "running",
                        };
                        yield Ok(Event::default().event("complete").data(
                            serde_json::json!({
                                "status": status,
                                "exit_code": task.exit_code
                            }).to_string()
                        ));
                    }
                    break;
                }
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("keepalive"),
    ))
}

/// 获取系统配置信息
/// GET /system/info
async fn get_system_info(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything()),
    );
    sys.refresh_cpu_all();

    let cpu_brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    let info = SystemInfo {
        hostname: System::host_name().unwrap_or_else(|| "unknown".to_string()),
        os_name: System::name().unwrap_or_else(|| "unknown".to_string()),
        os_version: System::os_version().unwrap_or_else(|| "unknown".to_string()),
        kernel_version: System::kernel_version().unwrap_or_else(|| "unknown".to_string()),
        cpu_arch: std::env::consts::ARCH.to_string(),
        cpu_count: sys.cpus().len(),
        cpu_brand,
        total_memory_gb: sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0,
        total_swap_gb: sys.total_swap() as f64 / 1024.0 / 1024.0 / 1024.0,
    };

    // 计算运行时间
    let uptime_secs = (chrono::Utc::now() - state.started_at).num_seconds();
    let uptime_str = format!(
        "{}d {}h {}m {}s",
        uptime_secs / 86400,
        (uptime_secs % 86400) / 3600,
        (uptime_secs % 3600) / 60,
        uptime_secs % 60
    );

    Json(serde_json::json!({
        "system": info,
        "agent": {
            "version": VERSION,
            "started_at": state.started_at.to_rfc3339(),
            "uptime": uptime_str,
            "projects": state.projects.keys().collect::<Vec<_>>(),
        }
    }))
}

/// 获取系统负载统计
/// GET /system/stats
async fn get_system_stats() -> impl IntoResponse {
    // 创建 System 实例并刷新所有需要的数据
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything()),
    );

    // 需要等待一小段时间让 CPU 使用率计算准确
    sys.refresh_cpu_all();
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    sys.refresh_cpu_all();

    // 获取磁盘信息
    let disks = Disks::new_with_refreshed_list();
    let disk_info: Vec<DiskInfo> = disks
        .iter()
        .map(|disk| {
            let total = disk.total_space() as f64 / 1024.0 / 1024.0 / 1024.0;
            let available = disk.available_space() as f64 / 1024.0 / 1024.0 / 1024.0;
            let used = total - available;
            DiskInfo {
                name: disk.name().to_string_lossy().to_string(),
                mount_point: disk.mount_point().to_string_lossy().to_string(),
                total_gb: total,
                used_gb: used,
                available_gb: available,
                usage_percent: if total > 0.0 {
                    (used / total) * 100.0
                } else {
                    0.0
                },
            }
        })
        .collect();

    // 计算 CPU 平均使用率
    let cpu_usage: f64 = if sys.cpus().is_empty() {
        0.0
    } else {
        sys.cpus().iter().map(|c| c.cpu_usage() as f64).sum::<f64>() / sys.cpus().len() as f64
    };

    let memory_total = sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
    let memory_used = sys.used_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
    let swap_total = sys.total_swap() as f64 / 1024.0 / 1024.0 / 1024.0;
    let swap_used = sys.used_swap() as f64 / 1024.0 / 1024.0 / 1024.0;

    // 获取负载平均值 (仅 Unix)
    let load_avg = System::load_average();

    let stats = SystemStats {
        timestamp: chrono::Utc::now(),
        cpu_usage_percent: cpu_usage,
        memory_used_gb: memory_used,
        memory_total_gb: memory_total,
        memory_usage_percent: if memory_total > 0.0 {
            (memory_used / memory_total) * 100.0
        } else {
            0.0
        },
        swap_used_gb: swap_used,
        swap_total_gb: swap_total,
        swap_usage_percent: if swap_total > 0.0 {
            (swap_used / swap_total) * 100.0
        } else {
            0.0
        },
        disks: disk_info,
        load_average: LoadAverage {
            one: load_avg.one,
            five: load_avg.five,
            fifteen: load_avg.fifteen,
        },
    };

    Json(stats)
}

/// 获取最近的任务历史
/// GET /tasks/recent
async fn get_recent_tasks(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TaskHistoryQuery>,
) -> impl IntoResponse {
    let history = state.task_history.read().await;
    let running_tasks = state.tasks.read().await;

    // 合并运行中的任务和历史任务
    let mut all_tasks: Vec<DeployTask> = running_tasks.values().cloned().collect();

    // 添加历史任务（按时间倒序）
    for task in history.iter().rev() {
        all_tasks.push(task.clone());
    }

    // 应用过滤器
    let filtered: Vec<DeployTask> = all_tasks
        .into_iter()
        .filter(|task| {
            // 项目过滤
            if let Some(ref project) = query.project {
                if &task.project != project {
                    return false;
                }
            }
            // 状态过滤
            if let Some(ref status) = query.status {
                let task_status = match task.status {
                    DeployStatus::Success => "success",
                    DeployStatus::Failed => "failed",
                    DeployStatus::Running => "running",
                };
                if task_status != status {
                    return false;
                }
            }
            true
        })
        .take(query.limit)
        .collect();

    let total = filtered.len();

    Json(TaskHistoryResponse {
        tasks: filtered,
        total,
    })
}

/// 列出所有 Docker 容器
/// GET /containers
async fn list_containers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    // 执行 docker ps -a 获取容器列表
    let output = Command::new("docker")
        .args(["ps", "-a", "--format", "{{.ID}}|{{.Names}}|{{.Image}}|{{.Status}}|{{.State}}|{{.CreatedAt}}|{{.Ports}}"])
        .output()
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to run docker ps");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "docker_error".to_string(),
                    message: format!("Failed to list containers: {}", e),
                }),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "docker_error".to_string(),
                message: format!("Docker command failed: {}", stderr),
            }),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let containers: Vec<ContainerInfo> = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split('|').collect();
            ContainerInfo {
                id: parts.get(0).unwrap_or(&"").to_string(),
                name: parts.get(1).unwrap_or(&"").to_string(),
                image: parts.get(2).unwrap_or(&"").to_string(),
                status: parts.get(3).unwrap_or(&"").to_string(),
                state: parts.get(4).unwrap_or(&"").to_string(),
                created: parts.get(5).unwrap_or(&"").to_string(),
                ports: parts.get(6).unwrap_or(&"").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            }
        })
        .collect();

    Ok(Json(ContainersResponse { containers }))
}

/// 获取容器日志
/// GET /containers/:name/logs
async fn get_container_logs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(container_name): Path<String>,
    Query(query): Query<ContainerLogsQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    let mut args = vec!["logs".to_string()];

    // 添加 tail 参数
    args.push("--tail".to_string());
    args.push(query.tail.to_string());

    // 添加 timestamps 参数
    if query.timestamps {
        args.push("--timestamps".to_string());
    }

    // 添加 since 参数
    if let Some(since) = &query.since {
        args.push("--since".to_string());
        args.push(since.clone());
    }

    args.push(container_name.clone());

    let output = Command::new("docker")
        .args(&args)
        .output()
        .await
        .map_err(|e| {
            error!(container = %container_name, error = %e, "Failed to get container logs");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "docker_error".to_string(),
                    message: format!("Failed to get logs: {}", e),
                }),
            )
        })?;

    // docker logs 输出可能同时在 stdout 和 stderr
    let mut logs: Vec<String> = Vec::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // 检查是否是错误（容器不存在等）
    if !output.status.success() && stderr.contains("No such container") {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".to_string(),
                message: format!("Container '{}' not found", container_name),
            }),
        ));
    }

    // 合并 stdout 和 stderr（docker logs 通常输出到 stderr）
    for line in stdout.lines() {
        if !line.is_empty() {
            logs.push(line.to_string());
        }
    }
    for line in stderr.lines() {
        if !line.is_empty() {
            logs.push(line.to_string());
        }
    }

    let total_lines = logs.len();

    Ok(Json(ContainerLogsResponse {
        container: container_name,
        logs,
        total_lines,
    }))
}

/// 获取容器环境变量
/// GET /containers/:name/env
async fn get_container_env(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(container_name): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    // 使用 docker inspect 获取容器环境变量
    let output = Command::new("docker")
        .args(["inspect", "--format", "{{range .Config.Env}}{{.}}\n{{end}}", &container_name])
        .output()
        .await
        .map_err(|e| {
            error!(container = %container_name, error = %e, "Failed to inspect container");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "docker_error".to_string(),
                    message: format!("Failed to get environment: {}", e),
                }),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such") {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "not_found".to_string(),
                    message: format!("Container '{}' not found", container_name),
                }),
            ));
        }
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "docker_error".to_string(),
                message: format!("Docker inspect failed: {}", stderr),
            }),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // 敏感关键词列表
    let sensitive_keywords = [
        "password", "secret", "key", "token", "credential", "auth",
        "api_key", "apikey", "private", "jwt", "pem", "cert",
    ];

    let env_vars: Vec<EnvVar> = stdout
        .lines()
        .filter(|line| !line.is_empty() && line.contains('='))
        .map(|line| {
            let mut parts = line.splitn(2, '=');
            let key = parts.next().unwrap_or("").to_string();
            let value = parts.next().unwrap_or("").to_string();

            // 检查是否是敏感信息
            let key_lower = key.to_lowercase();
            let sensitive = sensitive_keywords.iter().any(|kw| key_lower.contains(kw));

            EnvVar {
                key,
                value: if sensitive {
                    "***REDACTED***".to_string()
                } else {
                    value
                },
                sensitive,
            }
        })
        .collect();

    Ok(Json(ContainerEnvResponse {
        container: container_name,
        env_vars,
    }))
}

/// 获取容器完整环境变量（包括敏感信息，需要特殊权限）
/// GET /containers/:name/env/full
async fn get_container_env_full(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(container_name): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    // 使用 docker inspect 获取容器环境变量
    let output = Command::new("docker")
        .args(["inspect", "--format", "{{range .Config.Env}}{{.}}\n{{end}}", &container_name])
        .output()
        .await
        .map_err(|e| {
            error!(container = %container_name, error = %e, "Failed to inspect container");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "docker_error".to_string(),
                    message: format!("Failed to get environment: {}", e),
                }),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such") {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "not_found".to_string(),
                    message: format!("Container '{}' not found", container_name),
                }),
            ));
        }
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "docker_error".to_string(),
                message: format!("Docker inspect failed: {}", stderr),
            }),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // 敏感关键词列表
    let sensitive_keywords = [
        "password", "secret", "key", "token", "credential", "auth",
        "api_key", "apikey", "private", "jwt", "pem", "cert",
    ];

    let env_vars: Vec<EnvVar> = stdout
        .lines()
        .filter(|line| !line.is_empty() && line.contains('='))
        .map(|line| {
            let mut parts = line.splitn(2, '=');
            let key = parts.next().unwrap_or("").to_string();
            let value = parts.next().unwrap_or("").to_string();

            // 检查是否是敏感信息（但不隐藏）
            let key_lower = key.to_lowercase();
            let sensitive = sensitive_keywords.iter().any(|kw| key_lower.contains(kw));

            EnvVar {
                key,
                value,
                sensitive,
            }
        })
        .collect();

    Ok(Json(ContainerEnvResponse {
        container: container_name,
        env_vars,
    }))
}

// ============== 数据库管理端点 ==============

/// 获取数据库凭证（从配置或环境变量）
fn get_db_credentials(config: &DbConnectionConfig) -> (String, String) {
    let (default_user, env_user, env_pass) = match config.db_type {
        DatabaseType::Postgres => ("postgres", "POSTGRES_USER", "POSTGRES_PASSWORD"),
        DatabaseType::Mysql => ("root", "MYSQL_USER", "MYSQL_ROOT_PASSWORD"),
    };

    let username = config.username.clone()
        .or_else(|| env::var(env_user).ok())
        .unwrap_or_else(|| default_user.to_string());

    let password = config.password.clone()
        .or_else(|| env::var(env_pass).ok())
        .unwrap_or_default();

    (username, password)
}

/// 验证 SQL 是否安全（阻止危险操作）
fn validate_sql(sql: &str, allow_modify: bool) -> Result<(), String> {
    let sql_upper = sql.trim().to_uppercase();

    // 永远阻止的操作
    let blocked_patterns = [
        "DROP DATABASE",
        "DROP SCHEMA",
        "TRUNCATE",
        "ALTER USER",
        "CREATE USER",
        "DROP USER",
        "GRANT ",
        "REVOKE ",
        "\\!",        // MySQL shell escape
        "\\s",        // MySQL source file
        "pg_read_file",
        "pg_write_file",
        "COPY.*TO PROGRAM",
    ];

    for pattern in &blocked_patterns {
        if sql_upper.contains(pattern) {
            return Err(format!("Blocked operation detected: {}", pattern));
        }
    }

    // 如果不允许修改，检查修改关键字
    if !allow_modify {
        let modify_keywords = ["INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE", "REPLACE"];
        for keyword in &modify_keywords {
            if sql_upper.starts_with(keyword) {
                return Err(format!("Modification not allowed: {} statement", keyword));
            }
        }
    }

    Ok(())
}

/// 检测危险操作
fn is_dangerous_operation(sql: &str) -> (bool, Option<String>) {
    let sql_upper = sql.trim().to_uppercase();

    if sql_upper.contains("DELETE") && !sql_upper.contains("WHERE") {
        return (true, Some("DELETE without WHERE clause".to_string()));
    }
    if sql_upper.contains("UPDATE") && !sql_upper.contains("WHERE") {
        return (true, Some("UPDATE without WHERE clause".to_string()));
    }
    if sql_upper.starts_with("DROP TABLE") {
        return (true, Some("DROP TABLE operation".to_string()));
    }
    if sql_upper.starts_with("ALTER TABLE") && sql_upper.contains("DROP") {
        return (true, Some("ALTER TABLE DROP operation".to_string()));
    }

    (false, None)
}

/// POST /db/list - 列出数据库
async fn db_list_databases(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ListDatabasesRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    let mut databases = Vec::new();

    // 查询 PostgreSQL 数据库
    if req.db_type.is_none() || req.db_type == Some(DatabaseType::Postgres) {
        let pg_user = env::var("POSTGRES_USER").unwrap_or_else(|_| "postgres".to_string());
        let pg_pass = env::var("POSTGRES_PASSWORD").unwrap_or_default();
        let pg_container = env::var("POSTGRES_CONTAINER").unwrap_or_else(|_| "xjp-postgres".to_string());

        let output = Command::new("docker")
            .args([
                "exec",
                "-e", &format!("PGPASSWORD={}", pg_pass),
                &pg_container,
                "psql",
                "-U", &pg_user,
                "-d", "postgres",
                "-t", "-A",
                "-c", "SELECT datname, pg_database_size(datname)/1024/1024 as size_mb FROM pg_database WHERE datistemplate = false ORDER BY datname",
            ])
            .output()
            .await;

        if let Ok(output) = output {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if line.is_empty() { continue; }
                    let parts: Vec<&str> = line.split('|').collect();
                    if parts.len() >= 2 {
                        databases.push(DatabaseInfo {
                            name: parts[0].to_string(),
                            db_type: DatabaseType::Postgres,
                            container: pg_container.clone(),
                            size_mb: parts[1].parse().ok(),
                        });
                    }
                }
            }
        }
    }

    // 查询 MySQL 数据库
    if req.db_type.is_none() || req.db_type == Some(DatabaseType::Mysql) {
        let mysql_user = env::var("MYSQL_USER").unwrap_or_else(|_| "root".to_string());
        let mysql_pass = env::var("MYSQL_ROOT_PASSWORD").unwrap_or_default();
        let mysql_container = env::var("MYSQL_CONTAINER").unwrap_or_else(|_| "xjp-mysql".to_string());

        let output = Command::new("docker")
            .args([
                "exec",
                &mysql_container,
                "mysql",
                "-u", &mysql_user,
                &format!("-p{}", mysql_pass),
                "-N", "-B",
                "-e", "SELECT table_schema, ROUND(SUM(data_length + index_length)/1024/1024, 2) FROM information_schema.tables GROUP BY table_schema",
            ])
            .output()
            .await;

        if let Ok(output) = output {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if line.is_empty() { continue; }
                    let parts: Vec<&str> = line.split('\t').collect();
                    if parts.len() >= 2 {
                        // 跳过系统数据库
                        let db_name = parts[0];
                        if ["mysql", "information_schema", "performance_schema", "sys"].contains(&db_name) {
                            continue;
                        }
                        databases.push(DatabaseInfo {
                            name: db_name.to_string(),
                            db_type: DatabaseType::Mysql,
                            container: mysql_container.clone(),
                            size_mb: parts[1].parse().ok(),
                        });
                    }
                }
            }
        }
    }

    Ok(Json(ListDatabasesResponse { databases }))
}

/// POST /db/tables - 列出表
async fn db_list_tables(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ListTablesRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    let (username, password) = get_db_credentials(&req.connection);
    let schema = req.schema.unwrap_or_else(|| "public".to_string());

    let tables = match req.connection.db_type {
        DatabaseType::Postgres => {
            let sql = format!(
                "SELECT table_name, pg_total_relation_size(quote_ident(table_name))/1024 as size_kb \
                 FROM information_schema.tables \
                 WHERE table_schema = '{}' AND table_type = 'BASE TABLE' \
                 ORDER BY table_name",
                schema
            );

            let output = Command::new("docker")
                .args([
                    "exec",
                    "-e", &format!("PGPASSWORD={}", password),
                    &req.connection.container,
                    "psql",
                    "-U", &username,
                    "-d", &req.connection.database,
                    "-t", "-A", "-F", "|",
                    "-c", &sql,
                ])
                .output()
                .await
                .map_err(|e| (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "docker_error".to_string(),
                        message: format!("Failed to execute docker: {}", e),
                    }),
                ))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "db_error".to_string(),
                        message: format!("Database query failed: {}", stderr),
                    }),
                ));
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.lines()
                .filter(|line| !line.is_empty())
                .map(|line| {
                    let parts: Vec<&str> = line.split('|').collect();
                    TableInfo {
                        name: parts.first().unwrap_or(&"").to_string(),
                        schema: Some(schema.clone()),
                        row_count: None,
                        size_kb: parts.get(1).and_then(|s| s.parse().ok()),
                    }
                })
                .collect()
        }
        DatabaseType::Mysql => {
            let sql = format!(
                "SELECT table_name, ROUND((data_length + index_length)/1024, 2) as size_kb \
                 FROM information_schema.tables \
                 WHERE table_schema = '{}' \
                 ORDER BY table_name",
                req.connection.database
            );

            let output = Command::new("docker")
                .args([
                    "exec",
                    &req.connection.container,
                    "mysql",
                    "-u", &username,
                    &format!("-p{}", password),
                    "-N", "-B",
                    "-e", &sql,
                ])
                .output()
                .await
                .map_err(|e| (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "docker_error".to_string(),
                        message: format!("Failed to execute docker: {}", e),
                    }),
                ))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "db_error".to_string(),
                        message: format!("Database query failed: {}", stderr),
                    }),
                ));
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.lines()
                .filter(|line| !line.is_empty())
                .map(|line| {
                    let parts: Vec<&str> = line.split('\t').collect();
                    TableInfo {
                        name: parts.first().unwrap_or(&"").to_string(),
                        schema: None,
                        row_count: None,
                        size_kb: parts.get(1).and_then(|s| s.parse().ok()),
                    }
                })
                .collect()
        }
    };

    Ok(Json(ListTablesResponse {
        tables,
        database: req.connection.database,
    }))
}

/// POST /db/schema - 获取表结构
async fn db_get_schema(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<GetSchemaRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    let (username, password) = get_db_credentials(&req.connection);
    let schema = req.schema.unwrap_or_else(|| "public".to_string());

    let (columns, indexes) = match req.connection.db_type {
        DatabaseType::Postgres => {
            // 获取列信息
            let col_sql = format!(
                "SELECT c.column_name, c.data_type, c.is_nullable, c.column_default, \
                 CASE WHEN pk.column_name IS NOT NULL THEN 'YES' ELSE 'NO' END as is_pk \
                 FROM information_schema.columns c \
                 LEFT JOIN ( \
                     SELECT kcu.column_name FROM information_schema.table_constraints tc \
                     JOIN information_schema.key_column_usage kcu ON tc.constraint_name = kcu.constraint_name \
                     WHERE tc.table_name = '{}' AND tc.table_schema = '{}' AND tc.constraint_type = 'PRIMARY KEY' \
                 ) pk ON c.column_name = pk.column_name \
                 WHERE c.table_name = '{}' AND c.table_schema = '{}' \
                 ORDER BY c.ordinal_position",
                req.table, schema, req.table, schema
            );

            let col_output = Command::new("docker")
                .args([
                    "exec",
                    "-e", &format!("PGPASSWORD={}", password),
                    &req.connection.container,
                    "psql",
                    "-U", &username,
                    "-d", &req.connection.database,
                    "-t", "-A", "-F", "|",
                    "-c", &col_sql,
                ])
                .output()
                .await
                .map_err(|e| (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "docker_error".to_string(),
                        message: format!("Failed to execute docker: {}", e),
                    }),
                ))?;

            let columns: Vec<ColumnInfo> = if col_output.status.success() {
                String::from_utf8_lossy(&col_output.stdout)
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| {
                        let parts: Vec<&str> = line.split('|').collect();
                        ColumnInfo {
                            name: parts.first().unwrap_or(&"").to_string(),
                            data_type: parts.get(1).unwrap_or(&"").to_string(),
                            is_nullable: parts.get(2).map(|s| *s == "YES").unwrap_or(true),
                            default_value: parts.get(3).and_then(|s| if s.is_empty() { None } else { Some(s.to_string()) }),
                            is_primary_key: parts.get(4).map(|s| *s == "YES").unwrap_or(false),
                        }
                    })
                    .collect()
            } else {
                vec![]
            };

            // 获取索引信息
            let idx_sql = format!(
                "SELECT indexname, indexdef FROM pg_indexes \
                 WHERE schemaname = '{}' AND tablename = '{}' ORDER BY indexname",
                schema, req.table
            );

            let idx_output = Command::new("docker")
                .args([
                    "exec",
                    "-e", &format!("PGPASSWORD={}", password),
                    &req.connection.container,
                    "psql",
                    "-U", &username,
                    "-d", &req.connection.database,
                    "-t", "-A", "-F", "|",
                    "-c", &idx_sql,
                ])
                .output()
                .await
                .unwrap_or_else(|_| std::process::Output {
                    status: std::process::ExitStatus::default(),
                    stdout: vec![],
                    stderr: vec![],
                });

            let indexes: Vec<IndexInfo> = if idx_output.status.success() {
                String::from_utf8_lossy(&idx_output.stdout)
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| {
                        let parts: Vec<&str> = line.split('|').collect();
                        let idx_name = parts.first().unwrap_or(&"").to_string();
                        let idx_def = parts.get(1).unwrap_or(&"").to_string();
                        IndexInfo {
                            name: idx_name.clone(),
                            columns: vec![], // 从 idx_def 解析会比较复杂
                            is_unique: idx_def.contains("UNIQUE"),
                            is_primary: idx_name.ends_with("_pkey"),
                        }
                    })
                    .collect()
            } else {
                vec![]
            };

            (columns, indexes)
        }
        DatabaseType::Mysql => {
            // 获取列信息
            let col_sql = format!(
                "SELECT column_name, column_type, is_nullable, column_default, column_key \
                 FROM information_schema.columns \
                 WHERE table_schema = '{}' AND table_name = '{}' \
                 ORDER BY ordinal_position",
                req.connection.database, req.table
            );

            let col_output = Command::new("docker")
                .args([
                    "exec",
                    &req.connection.container,
                    "mysql",
                    "-u", &username,
                    &format!("-p{}", password),
                    "-N", "-B",
                    "-e", &col_sql,
                ])
                .output()
                .await
                .map_err(|e| (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "docker_error".to_string(),
                        message: format!("Failed to execute docker: {}", e),
                    }),
                ))?;

            let columns: Vec<ColumnInfo> = if col_output.status.success() {
                String::from_utf8_lossy(&col_output.stdout)
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| {
                        let parts: Vec<&str> = line.split('\t').collect();
                        ColumnInfo {
                            name: parts.first().unwrap_or(&"").to_string(),
                            data_type: parts.get(1).unwrap_or(&"").to_string(),
                            is_nullable: parts.get(2).map(|s| *s == "YES").unwrap_or(true),
                            default_value: parts.get(3).and_then(|s| if s.is_empty() || *s == "NULL" { None } else { Some(s.to_string()) }),
                            is_primary_key: parts.get(4).map(|s| *s == "PRI").unwrap_or(false),
                        }
                    })
                    .collect()
            } else {
                vec![]
            };

            // 获取索引信息
            let idx_sql = format!("SHOW INDEX FROM {}", req.table);

            let idx_output = Command::new("docker")
                .args([
                    "exec",
                    &req.connection.container,
                    "mysql",
                    "-u", &username,
                    &format!("-p{}", password),
                    "-D", &req.connection.database,
                    "-N", "-B",
                    "-e", &idx_sql,
                ])
                .output()
                .await
                .unwrap_or_else(|_| std::process::Output {
                    status: std::process::ExitStatus::default(),
                    stdout: vec![],
                    stderr: vec![],
                });

            let indexes: Vec<IndexInfo> = if idx_output.status.success() {
                let mut idx_map: std::collections::HashMap<String, IndexInfo> = std::collections::HashMap::new();
                for line in String::from_utf8_lossy(&idx_output.stdout).lines() {
                    if line.is_empty() { continue; }
                    let parts: Vec<&str> = line.split('\t').collect();
                    if parts.len() >= 5 {
                        let key_name = parts.get(2).unwrap_or(&"").to_string();
                        let col_name = parts.get(4).unwrap_or(&"").to_string();
                        let non_unique = parts.get(1).unwrap_or(&"1") == &"0";

                        idx_map.entry(key_name.clone())
                            .and_modify(|idx| idx.columns.push(col_name.clone()))
                            .or_insert_with(|| IndexInfo {
                                name: key_name.clone(),
                                columns: vec![col_name],
                                is_unique: non_unique,
                                is_primary: key_name == "PRIMARY",
                            });
                    }
                }
                idx_map.into_values().collect()
            } else {
                vec![]
            };

            (columns, indexes)
        }
    };

    Ok(Json(GetSchemaResponse {
        table: req.table,
        columns,
        indexes,
        row_count: None,
    }))
}

/// POST /db/query - 执行 SELECT 查询
async fn db_execute_query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<DbQueryRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    // 验证 SQL（只允许查询）
    if let Err(e) = validate_sql(&req.sql, false) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "invalid_sql".to_string(),
                message: e,
            }),
        ));
    }

    let (username, password) = get_db_credentials(&req.connection);
    let start = std::time::Instant::now();

    // 添加 LIMIT
    let sql = if !req.sql.to_uppercase().contains("LIMIT") {
        format!("{} LIMIT {}", req.sql.trim_end_matches(';'), req.max_rows)
    } else {
        req.sql.clone()
    };

    let output = match req.connection.db_type {
        DatabaseType::Postgres => {
            Command::new("docker")
                .args([
                    "exec",
                    "-e", &format!("PGPASSWORD={}", password),
                    &req.connection.container,
                    "psql",
                    "-U", &username,
                    "-d", &req.connection.database,
                    "-t", "-A", "-F", "\t",
                    "-c", &sql,
                ])
                .output()
                .await
        }
        DatabaseType::Mysql => {
            Command::new("docker")
                .args([
                    "exec",
                    &req.connection.container,
                    "mysql",
                    "-u", &username,
                    &format!("-p{}", password),
                    "-D", &req.connection.database,
                    "-N", "-B",
                    "-e", &sql,
                ])
                .output()
                .await
        }
    }.map_err(|e| (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: "docker_error".to_string(),
            message: format!("Failed to execute docker: {}", e),
        }),
    ))?;

    let execution_time_ms = start.elapsed().as_millis() as u64;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "query_error".to_string(),
                message: format!("Query failed: {}", stderr),
            }),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let rows: Vec<Vec<serde_json::Value>> = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split('\t')
                .map(|val| {
                    if val.is_empty() || val == "NULL" {
                        serde_json::Value::Null
                    } else if let Ok(n) = val.parse::<i64>() {
                        serde_json::Value::Number(n.into())
                    } else if let Ok(n) = val.parse::<f64>() {
                        serde_json::json!(n)
                    } else if val == "t" || val == "true" {
                        serde_json::Value::Bool(true)
                    } else if val == "f" || val == "false" {
                        serde_json::Value::Bool(false)
                    } else {
                        serde_json::Value::String(val.to_string())
                    }
                })
                .collect()
        })
        .collect();

    let row_count = rows.len();
    let warning = if row_count >= req.max_rows {
        Some(format!("Results may be truncated at {} rows", req.max_rows))
    } else {
        None
    };

    Ok(Json(DbQueryResponse {
        success: true,
        columns: vec![], // 需要额外查询获取列名
        rows,
        row_count,
        execution_time_ms,
        warning,
    }))
}

/// POST /db/execute - 执行 INSERT/UPDATE/DELETE
async fn db_execute_command(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<DbExecuteRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    // 验证 SQL
    if let Err(e) = validate_sql(&req.sql, true) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "invalid_sql".to_string(),
                message: e,
            }),
        ));
    }

    // 检查危险操作
    let (is_dangerous, reason) = is_dangerous_operation(&req.sql);
    if is_dangerous && !req.confirm_dangerous {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "dangerous_operation".to_string(),
                message: format!("Dangerous operation detected: {}. Set confirm_dangerous=true to proceed.", reason.unwrap_or_default()),
            }),
        ));
    }

    let (username, password) = get_db_credentials(&req.connection);
    let start = std::time::Instant::now();

    let output = match req.connection.db_type {
        DatabaseType::Postgres => {
            Command::new("docker")
                .args([
                    "exec",
                    "-e", &format!("PGPASSWORD={}", password),
                    &req.connection.container,
                    "psql",
                    "-U", &username,
                    "-d", &req.connection.database,
                    "-t", "-A",
                    "-c", &req.sql,
                ])
                .output()
                .await
        }
        DatabaseType::Mysql => {
            Command::new("docker")
                .args([
                    "exec",
                    &req.connection.container,
                    "mysql",
                    "-u", &username,
                    &format!("-p{}", password),
                    "-D", &req.connection.database,
                    "-e", &req.sql,
                ])
                .output()
                .await
        }
    }.map_err(|e| (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: "docker_error".to_string(),
            message: format!("Failed to execute docker: {}", e),
        }),
    ))?;

    let execution_time_ms = start.elapsed().as_millis() as u64;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(Json(DbExecuteResponse {
            success: false,
            affected_rows: 0,
            execution_time_ms,
            warning: None,
            error: Some(stderr.to_string()),
        }));
    }

    // 尝试解析受影响的行数
    let stdout = String::from_utf8_lossy(&output.stdout);
    let affected_rows = if req.connection.db_type == DatabaseType::Postgres {
        // PostgreSQL 输出类似 "UPDATE 5" 或 "INSERT 0 5"
        stdout.split_whitespace()
            .last()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    } else {
        // MySQL 不直接输出行数，默认 0
        0
    };

    Ok(Json(DbExecuteResponse {
        success: true,
        affected_rows,
        execution_time_ms,
        warning: if is_dangerous { reason } else { None },
        error: None,
    }))
}

/// POST /db/export - 导出数据
async fn db_export_data(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<DbExportRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    if !state.verify_api_key(&headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".to_string(),
                message: "Invalid or missing API key".to_string(),
            }),
        ));
    }

    let (username, password) = get_db_credentials(&req.connection);
    let start = std::time::Instant::now();

    // 构建查询
    let sql = if req.is_query {
        if !req.source.to_uppercase().contains("LIMIT") {
            format!("{} LIMIT {}", req.source.trim_end_matches(';'), req.max_rows)
        } else {
            req.source.clone()
        }
    } else {
        format!("SELECT * FROM {} LIMIT {}", req.source, req.max_rows)
    };

    // 验证 SQL
    if let Err(e) = validate_sql(&sql, false) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "invalid_sql".to_string(),
                message: e,
            }),
        ));
    }

    let output = match req.connection.db_type {
        DatabaseType::Postgres => {
            match req.format {
                ExportFormat::Csv => {
                    Command::new("docker")
                        .args([
                            "exec",
                            "-e", &format!("PGPASSWORD={}", password),
                            &req.connection.container,
                            "psql",
                            "-U", &username,
                            "-d", &req.connection.database,
                            "-c", &format!("COPY ({}) TO STDOUT WITH CSV HEADER", sql),
                        ])
                        .output()
                        .await
                }
                ExportFormat::Json => {
                    Command::new("docker")
                        .args([
                            "exec",
                            "-e", &format!("PGPASSWORD={}", password),
                            &req.connection.container,
                            "psql",
                            "-U", &username,
                            "-d", &req.connection.database,
                            "-t", "-A",
                            "-c", &format!("SELECT row_to_json(t) FROM ({}) t", sql),
                        ])
                        .output()
                        .await
                }
                ExportFormat::Sql => {
                    // SQL 格式导出整个表
                    Command::new("docker")
                        .args([
                            "exec",
                            "-e", &format!("PGPASSWORD={}", password),
                            &req.connection.container,
                            "pg_dump",
                            "-U", &username,
                            "-d", &req.connection.database,
                            "-t", &req.source,
                            "--data-only",
                            "--inserts",
                        ])
                        .output()
                        .await
                }
            }
        }
        DatabaseType::Mysql => {
            match req.format {
                ExportFormat::Csv => {
                    // MySQL CSV 导出
                    let csv_sql = format!(
                        "{} INTO OUTFILE '/tmp/export.csv' \
                         FIELDS TERMINATED BY ',' ENCLOSED BY '\"' \
                         LINES TERMINATED BY '\\n'",
                        sql
                    );
                    // 由于权限问题，使用简单的 tab 分隔输出
                    Command::new("docker")
                        .args([
                            "exec",
                            &req.connection.container,
                            "mysql",
                            "-u", &username,
                            &format!("-p{}", password),
                            "-D", &req.connection.database,
                            "-B",
                            "-e", &sql,
                        ])
                        .output()
                        .await
                }
                ExportFormat::Json => {
                    Command::new("docker")
                        .args([
                            "exec",
                            &req.connection.container,
                            "mysql",
                            "-u", &username,
                            &format!("-p{}", password),
                            "-D", &req.connection.database,
                            "-N", "-B",
                            "-e", &sql,
                        ])
                        .output()
                        .await
                }
                ExportFormat::Sql => {
                    Command::new("docker")
                        .args([
                            "exec",
                            &req.connection.container,
                            "mysqldump",
                            "-u", &username,
                            &format!("-p{}", password),
                            &req.connection.database,
                            &req.source,
                            "--no-create-info",
                        ])
                        .output()
                        .await
                }
            }
        }
    }.map_err(|e| (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: "docker_error".to_string(),
            message: format!("Failed to execute docker: {}", e),
        }),
    ))?;

    let execution_time_ms = start.elapsed().as_millis() as u64;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "export_error".to_string(),
                message: format!("Export failed: {}", stderr),
            }),
        ));
    }

    let data = String::from_utf8_lossy(&output.stdout).to_string();
    let row_count = data.lines().count().saturating_sub(1); // 减去 header

    Ok(Json(DbExportResponse {
        success: true,
        format: req.format,
        row_count,
        data,
        execution_time_ms,
        warning: if row_count >= req.max_rows {
            Some(format!("Results may be truncated at {} rows", req.max_rows))
        } else {
            None
        },
    }))
}

// ============== 数据库管理端点结束 ==============

// ============== 隧道端点 ==============

/// 获取隧道状态
async fn get_tunnel_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !state.verify_api_key(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "Unauthorized"}))).into_response();
    }

    let response = match &state.tunnel_mode {
        TunnelMode::Server => {
            let server_state = &state.tunnel_server_state;
            let client_connected = *server_state.client_connected.read().await;
            let client_addr = server_state.client_addr.read().await.clone();
            let client_connected_at = *server_state.client_connected_at.read().await;
            let client_mappings = server_state.client_mappings.read().await.clone();
            let port_listeners = server_state.port_listeners.read().await;

            let port_mappings: Vec<PortMappingStatus> = client_mappings
                .iter()
                .map(|m| PortMappingStatus {
                    mapping: m.clone(),
                    status: if port_listeners.contains_key(&m.remote_port) {
                        "listening".to_string()
                    } else {
                        "pending".to_string()
                    },
                    active_connections: 0, // TODO: 统计活跃连接数
                })
                .collect();

            TunnelStatusResponse::Server(TunnelServerStatus {
                listening: true,
                listen_port: env::var("PORT").unwrap_or_else(|_| "9876".to_string()).parse().unwrap_or(9876),
                client_connected,
                client_addr,
                client_connected_at,
                port_mappings,
            })
        }
        TunnelMode::Client => {
            let client_state = &state.tunnel_client_state;
            let connected = *client_state.connected.read().await;
            let connected_at = *client_state.connected_at.read().await;
            let last_error = client_state.last_error.read().await.clone();
            let reconnect_count = *client_state.reconnect_count.read().await;

            let mappings: Vec<PortMappingStatus> = state
                .tunnel_port_mappings
                .iter()
                .map(|m| PortMappingStatus {
                    mapping: m.clone(),
                    status: if connected { "active".to_string() } else { "disconnected".to_string() },
                    active_connections: 0,
                })
                .collect();

            TunnelStatusResponse::Client(TunnelClientStatus {
                connected,
                server_url: state.tunnel_server_url.clone(),
                connected_at,
                last_error,
                reconnect_count,
                mappings,
            })
        }
        TunnelMode::Disabled => TunnelStatusResponse::Disabled,
    };

    Json(response).into_response()
}

/// WebSocket 隧道端点 (Server 模式)
async fn tunnel_websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // 验证 token
    let token = headers
        .get("x-tunnel-token")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            // 也检查 query parameter
            headers
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok())
        });

    if token != Some(&state.tunnel_auth_token) {
        return (StatusCode::UNAUTHORIZED, "Invalid tunnel token").into_response();
    }

    if state.tunnel_mode != TunnelMode::Server {
        return (StatusCode::BAD_REQUEST, "Tunnel server mode not enabled").into_response();
    }

    // 获取客户端地址
    let client_addr = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    ws.on_upgrade(move |socket| handle_tunnel_server_connection(socket, state, client_addr))
}

/// 处理 Server 端的隧道连接
async fn handle_tunnel_server_connection(
    socket: WebSocket,
    state: Arc<AppState>,
    client_addr: String,
) {
    info!(client_addr = %client_addr, "Tunnel client connected");

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let server_state = &state.tunnel_server_state;

    // 创建发送通道
    let (msg_tx, mut msg_rx) = mpsc::channel::<TunnelMessage>(256);

    // 更新连接状态
    {
        *server_state.client_connected.write().await = true;
        *server_state.client_addr.write().await = Some(client_addr.clone());
        *server_state.client_connected_at.write().await = Some(chrono::Utc::now());
        *server_state.ws_tx.write().await = Some(msg_tx.clone());
    }

    // 启动发送任务
    let send_task = tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            let data = match bincode::serialize(&msg) {
                Ok(d) => d,
                Err(e) => {
                    error!("Failed to serialize tunnel message: {}", e);
                    continue;
                }
            };
            if ws_sender.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    // 接收和处理消息
    while let Some(msg) = ws_receiver.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                match bincode::deserialize::<TunnelMessage>(&data) {
                    Ok(tunnel_msg) => {
                        handle_server_tunnel_message(tunnel_msg, &state, &msg_tx).await;
                    }
                    Err(e) => {
                        error!("Failed to deserialize tunnel message: {}", e);
                    }
                }
            }
            Ok(Message::Ping(data)) => {
                // 自动由 axum 处理 pong
                let _ = data;
            }
            Ok(Message::Close(_)) => {
                info!("Tunnel client disconnected");
                break;
            }
            Err(e) => {
                error!("WebSocket error: {}", e);
                break;
            }
            _ => {}
        }
    }

    // 清理连接状态
    {
        *server_state.client_connected.write().await = false;
        *server_state.client_addr.write().await = None;
        *server_state.ws_tx.write().await = None;

        // 停止所有端口监听器
        let mut listeners = server_state.port_listeners.write().await;
        for (port, cancel) in listeners.drain() {
            info!(port = port, "Stopping port listener");
            cancel.cancel();
        }

        // 清理代理连接
        server_state.proxy_connections.write().await.clear();
        server_state.client_mappings.write().await.clear();
    }

    send_task.abort();
    info!("Tunnel connection cleanup complete");
}

/// 处理 Server 端收到的隧道消息
async fn handle_server_tunnel_message(
    msg: TunnelMessage,
    state: &Arc<AppState>,
    ws_tx: &mpsc::Sender<TunnelMessage>,
) {
    let server_state = &state.tunnel_server_state;

    match msg {
        TunnelMessage::Config { mappings } => {
            info!(mappings = ?mappings, "Received port mappings from client");

            // 保存映射配置
            *server_state.client_mappings.write().await = mappings.clone();

            // 为每个映射启动端口监听器
            for mapping in mappings {
                let port = mapping.remote_port;
                let state_clone = state.clone();
                let ws_tx_clone = ws_tx.clone();
                let cancel = CancellationToken::new();
                let cancel_clone = cancel.clone();

                server_state.port_listeners.write().await.insert(port, cancel);

                tokio::spawn(async move {
                    start_port_listener(port, state_clone, ws_tx_clone, cancel_clone).await;
                });
            }
        }
        TunnelMessage::Connected { conn_id } => {
            info!(conn_id = %conn_id, "Client confirmed connection");
        }
        TunnelMessage::ConnectFailed { conn_id, error } => {
            warn!(conn_id = %conn_id, error = %error, "Client failed to connect to local service");
            // 清理代理连接
            server_state.proxy_connections.write().await.remove(&conn_id);
        }
        TunnelMessage::Data { conn_id, data } => {
            // 将数据转发到对应的代理连接
            let connections = server_state.proxy_connections.read().await;
            if let Some(tx) = connections.get(&conn_id) {
                if tx.send(data).await.is_err() {
                    drop(connections);
                    server_state.proxy_connections.write().await.remove(&conn_id);
                }
            }
        }
        TunnelMessage::Close { conn_id } => {
            info!(conn_id = %conn_id, "Client closed connection");
            server_state.proxy_connections.write().await.remove(&conn_id);
        }
        TunnelMessage::Pong => {
            // 心跳响应
        }
        _ => {}
    }
}

/// 启动端口监听器 (Server 模式)
async fn start_port_listener(
    port: u16,
    state: Arc<AppState>,
    ws_tx: mpsc::Sender<TunnelMessage>,
    cancel: CancellationToken,
) {
    let addr = format!("0.0.0.0:{}", port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(port = port, error = %e, "Failed to bind port listener");
            return;
        }
    };

    info!(port = port, "Port listener started");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(port = port, "Port listener cancelled");
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, remote_addr)) => {
                        let conn_id = Uuid::new_v4().to_string();
                        info!(
                            conn_id = %conn_id,
                            port = port,
                            remote_addr = %remote_addr,
                            "New connection on tunnel port"
                        );

                        // 创建代理连接通道
                        let (proxy_tx, proxy_rx) = mpsc::channel::<Vec<u8>>(256);

                        // 保存代理连接
                        state.tunnel_server_state.proxy_connections.write().await
                            .insert(conn_id.clone(), proxy_tx);

                        // 发送连接请求到客户端
                        let _ = ws_tx.send(TunnelMessage::Connect {
                            conn_id: conn_id.clone(),
                            remote_port: port,
                        }).await;

                        // 启动数据转发任务
                        let ws_tx_clone = ws_tx.clone();
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            handle_proxy_connection(
                                conn_id,
                                stream,
                                proxy_rx,
                                ws_tx_clone,
                                state_clone,
                            ).await;
                        });
                    }
                    Err(e) => {
                        error!(port = port, error = %e, "Failed to accept connection");
                    }
                }
            }
        }
    }
}

/// 处理代理连接的数据转发 (Server 模式)
async fn handle_proxy_connection(
    conn_id: String,
    stream: TcpStream,
    mut proxy_rx: mpsc::Receiver<Vec<u8>>,
    ws_tx: mpsc::Sender<TunnelMessage>,
    state: Arc<AppState>,
) {
    let (mut reader, mut writer) = stream.into_split();

    // 从 TCP 读取并发送到隧道
    let conn_id_read = conn_id.clone();
    let ws_tx_read = ws_tx.clone();
    let read_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 32768];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    if ws_tx_read.send(TunnelMessage::Data {
                        conn_id: conn_id_read.clone(),
                        data,
                    }).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    error!(conn_id = %conn_id_read, error = %e, "Read error");
                    break;
                }
            }
        }
        // 发送关闭消息
        let _ = ws_tx_read.send(TunnelMessage::Close {
            conn_id: conn_id_read,
        }).await;
    });

    // 从隧道接收并写入 TCP
    let _conn_id_write = conn_id.clone();
    let write_task = tokio::spawn(async move {
        while let Some(data) = proxy_rx.recv().await {
            if writer.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    // 等待任一任务完成
    tokio::select! {
        _ = read_task => {}
        _ = write_task => {}
    }

    // 清理
    state.tunnel_server_state.proxy_connections.write().await.remove(&conn_id);
}

/// 启动隧道客户端 (Client 模式)
async fn start_tunnel_client(state: Arc<AppState>) {
    let server_url = state.tunnel_server_url.clone();
    let reconnect_interval = env::var("TUNNEL_RECONNECT_INTERVAL")
        .unwrap_or_else(|_| "5".to_string())
        .parse::<u64>()
        .unwrap_or(5);

    loop {
        info!(server_url = %server_url, "Connecting to tunnel server");

        match connect_tunnel_client(&state).await {
            Ok(_) => {
                warn!("Tunnel connection closed, will reconnect");
            }
            Err(e) => {
                error!(error = %e, "Tunnel connection failed");
                let mut last_error = state.tunnel_client_state.last_error.write().await;
                *last_error = Some(e.to_string());
            }
        }

        // 更新状态
        {
            *state.tunnel_client_state.connected.write().await = false;
            *state.tunnel_client_state.connected_at.write().await = None;
            let mut count = state.tunnel_client_state.reconnect_count.write().await;
            *count += 1;
        }

        tokio::time::sleep(Duration::from_secs(reconnect_interval)).await;
    }
}

/// 连接到隧道服务端
async fn connect_tunnel_client(state: &Arc<AppState>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = url::Url::parse(&state.tunnel_server_url)?;

    // 构建带认证的请求
    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(state.tunnel_server_url.as_str())
        .header("x-tunnel-token", &state.tunnel_auth_token)
        .header("Host", url.host_str().unwrap_or("localhost"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tokio_tungstenite::tungstenite::handshake::client::generate_key())
        .body(())?;

    let (ws_stream, _) = connect_async(request).await?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // 更新连接状态
    {
        *state.tunnel_client_state.connected.write().await = true;
        *state.tunnel_client_state.connected_at.write().await = Some(chrono::Utc::now());
        *state.tunnel_client_state.last_error.write().await = None;
    }

    info!("Connected to tunnel server");

    // 发送端口映射配置
    let config_msg = TunnelMessage::Config {
        mappings: state.tunnel_port_mappings.clone(),
    };
    let config_data = bincode::serialize(&config_msg)?;
    ws_sender.send(TungsteniteMessage::Binary(config_data)).await?;

    info!(mappings = ?state.tunnel_port_mappings, "Sent port mappings to server");

    // 创建发送通道
    let (msg_tx, mut msg_rx) = mpsc::channel::<TunnelMessage>(256);

    // 启动发送任务
    let send_task = tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            let data = match bincode::serialize(&msg) {
                Ok(d) => d,
                Err(_) => continue,
            };
            if ws_sender.send(TungsteniteMessage::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    // 处理消息
    while let Some(msg) = ws_receiver.next().await {
        match msg? {
            TungsteniteMessage::Binary(data) => {
                let tunnel_msg: TunnelMessage = bincode::deserialize(&data)?;
                handle_client_tunnel_message(tunnel_msg, state, &msg_tx).await;
            }
            TungsteniteMessage::Ping(data) => {
                // tungstenite 自动处理 pong
                let _ = data;
            }
            TungsteniteMessage::Close(_) => {
                info!("Server closed connection");
                break;
            }
            _ => {}
        }
    }

    send_task.abort();
    Ok(())
}

/// 处理 Client 端收到的隧道消息
async fn handle_client_tunnel_message(
    msg: TunnelMessage,
    state: &Arc<AppState>,
    ws_tx: &mpsc::Sender<TunnelMessage>,
) {
    match msg {
        TunnelMessage::Connect { conn_id, remote_port } => {
            info!(conn_id = %conn_id, remote_port = remote_port, "Server requests connection");

            // 找到对应的本地映射
            let mapping = state.tunnel_port_mappings.iter()
                .find(|m| m.remote_port == remote_port);

            let mapping = match mapping {
                Some(m) => m.clone(),
                None => {
                    let _ = ws_tx.send(TunnelMessage::ConnectFailed {
                        conn_id,
                        error: format!("No mapping for port {}", remote_port),
                    }).await;
                    return;
                }
            };

            // 连接到本地服务
            let local_addr = format!("{}:{}", mapping.local_host, mapping.local_port);
            let local_stream = match TcpStream::connect(&local_addr).await {
                Ok(s) => s,
                Err(e) => {
                    error!(
                        conn_id = %conn_id,
                        local_addr = %local_addr,
                        error = %e,
                        "Failed to connect to local service"
                    );
                    let _ = ws_tx.send(TunnelMessage::ConnectFailed {
                        conn_id,
                        error: e.to_string(),
                    }).await;
                    return;
                }
            };

            info!(conn_id = %conn_id, local_addr = %local_addr, "Connected to local service");

            // 发送确认
            let _ = ws_tx.send(TunnelMessage::Connected {
                conn_id: conn_id.clone(),
            }).await;

            // 创建本地连接通道
            let (local_tx, local_rx) = mpsc::channel::<Vec<u8>>(256);
            state.tunnel_client_state.local_connections.write().await
                .insert(conn_id.clone(), local_tx);

            // 启动数据转发
            let ws_tx_clone = ws_tx.clone();
            let state_clone = state.clone();
            tokio::spawn(async move {
                handle_local_proxy_connection(
                    conn_id,
                    local_stream,
                    local_rx,
                    ws_tx_clone,
                    state_clone,
                ).await;
            });
        }
        TunnelMessage::Data { conn_id, data } => {
            // 转发到本地连接
            let connections = state.tunnel_client_state.local_connections.read().await;
            if let Some(tx) = connections.get(&conn_id) {
                if tx.send(data).await.is_err() {
                    drop(connections);
                    state.tunnel_client_state.local_connections.write().await.remove(&conn_id);
                }
            }
        }
        TunnelMessage::Close { conn_id } => {
            info!(conn_id = %conn_id, "Server closed connection");
            state.tunnel_client_state.local_connections.write().await.remove(&conn_id);
        }
        TunnelMessage::Ping => {
            let _ = ws_tx.send(TunnelMessage::Pong).await;
        }
        _ => {}
    }
}

/// 处理本地代理连接的数据转发 (Client 模式)
async fn handle_local_proxy_connection(
    conn_id: String,
    stream: TcpStream,
    mut local_rx: mpsc::Receiver<Vec<u8>>,
    ws_tx: mpsc::Sender<TunnelMessage>,
    state: Arc<AppState>,
) {
    let (mut reader, mut writer) = stream.into_split();

    // 从本地服务读取并发送到隧道
    let conn_id_read = conn_id.clone();
    let ws_tx_read = ws_tx.clone();
    let read_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 32768];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    if ws_tx_read.send(TunnelMessage::Data {
                        conn_id: conn_id_read.clone(),
                        data,
                    }).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = ws_tx_read.send(TunnelMessage::Close {
            conn_id: conn_id_read,
        }).await;
    });

    // 从隧道接收并写入本地服务
    let write_task = tokio::spawn(async move {
        while let Some(data) = local_rx.recv().await {
            if writer.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = read_task => {}
        _ = write_task => {}
    }

    state.tunnel_client_state.local_connections.write().await.remove(&conn_id);
}

// ============== 隧道端点结束 ==============

// ============== 自动更新功能 ==============

/// 启动自动更新后台任务
async fn start_auto_update_task(state: Arc<AppState>) {
    let config = match &state.auto_update_config {
        Some(c) => c.clone(),
        None => return,
    };

    info!("Starting auto-update background task");

    // 启动时立即检查一次
    tokio::time::sleep(Duration::from_secs(10)).await;
    check_and_apply_update(&state, &config).await;

    // 定期检查
    let interval = Duration::from_secs(config.check_interval_secs);
    loop {
        // 更新下一次检查时间
        let next = chrono::Utc::now() + chrono::Duration::seconds(config.check_interval_secs as i64);
        *state.auto_update_state.next_check.write().await = Some(next);

        tokio::time::sleep(interval).await;
        check_and_apply_update(&state, &config).await;
    }
}

/// 检查并应用更新
async fn check_and_apply_update(state: &Arc<AppState>, config: &AutoUpdateConfig) {
    info!("Checking for updates...");
    *state.auto_update_state.last_check.write().await = Some(chrono::Utc::now());

    // 获取最新版本信息
    let metadata_url = format!("{}/{}", config.endpoint, config.metadata_path);
    let metadata = match fetch_update_metadata(&metadata_url).await {
        Ok(m) => m,
        Err(e) => {
            let msg = format!("Failed to fetch metadata: {}", e);
            warn!("{}", msg);
            *state.auto_update_state.last_check_result.write().await = Some(msg);
            return;
        }
    };

    *state.auto_update_state.latest_version.write().await = Some(metadata.version.clone());

    // 比较版本
    if metadata.version == VERSION {
        *state.auto_update_state.update_available.write().await = false;
        *state.auto_update_state.last_check_result.write().await = Some("Already at latest version".to_string());
        info!("Already at latest version: {}", VERSION);
        return;
    }

    // 检查是否为更新版本（简单字符串比较，假设语义化版本）
    if !is_newer_version(&metadata.version, VERSION) {
        *state.auto_update_state.update_available.write().await = false;
        *state.auto_update_state.last_check_result.write().await = Some(format!("Current {} >= remote {}", VERSION, metadata.version));
        info!("Current version {} is not older than {}", VERSION, metadata.version);
        return;
    }

    info!("New version available: {} (current: {})", metadata.version, VERSION);
    *state.auto_update_state.update_available.write().await = true;
    *state.auto_update_state.last_check_result.write().await = Some(format!("Update available: {}", metadata.version));

    // 下载并应用更新
    if let Err(e) = download_and_apply_update(state, config, &metadata).await {
        error!("Failed to apply update: {}", e);
        *state.auto_update_state.update_progress.write().await = "failed".to_string();
        *state.auto_update_state.last_check_result.write().await = Some(format!("Update failed: {}", e));
    }
}

/// 获取更新元数据
async fn fetch_update_metadata(url: &str) -> Result<UpdateMetadata, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client.get(url)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    resp.json::<UpdateMetadata>()
        .await
        .map_err(|e| format!("JSON parse failed: {}", e))
}

/// 比较版本号（简单实现）
fn is_newer_version(new: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> {
        v.split('.')
            .filter_map(|s| s.parse().ok())
            .collect()
    };

    let new_parts = parse(new);
    let current_parts = parse(current);

    for i in 0..std::cmp::max(new_parts.len(), current_parts.len()) {
        let n = new_parts.get(i).copied().unwrap_or(0);
        let c = current_parts.get(i).copied().unwrap_or(0);
        if n > c {
            return true;
        }
        if n < c {
            return false;
        }
    }
    false
}

/// 下载并应用更新
async fn download_and_apply_update(
    state: &Arc<AppState>,
    config: &AutoUpdateConfig,
    metadata: &UpdateMetadata,
) -> Result<(), String> {
    *state.auto_update_state.update_progress.write().await = "downloading".to_string();

    // 构建二进制 URL
    let binary_name = config.binary_path_template.replace("{version}", &metadata.version);
    let binary_url = format!("{}/{}", config.endpoint, binary_name);

    info!("Downloading update from: {}", binary_url);

    // 下载二进制
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client.get(&binary_url)
        .send()
        .await
        .map_err(|e| format!("Download failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Download failed: HTTP {}", resp.status()));
    }

    let bytes = resp.bytes()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;

    info!("Downloaded {} bytes", bytes.len());

    // 验证 SHA256
    *state.auto_update_state.update_progress.write().await = "verifying".to_string();
    if let Some(expected_sha256) = &metadata.sha256 {
        use sha2::{Sha256, Digest};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let result = hasher.finalize();
        let actual_sha256 = hex::encode(result);

        if actual_sha256 != *expected_sha256 {
            return Err(format!("SHA256 mismatch: expected {}, got {}", expected_sha256, actual_sha256));
        }
        info!("SHA256 verified");
    }

    // 应用更新
    *state.auto_update_state.update_progress.write().await = "applying".to_string();

    // 获取当前二进制路径
    let current_exe = std::env::current_exe()
        .map_err(|e| format!("Failed to get current exe path: {}", e))?;

    let backup_path = current_exe.with_extension("bak");
    let new_path = current_exe.with_extension("new");

    // 写入新二进制
    std::fs::write(&new_path, &bytes)
        .map_err(|e| format!("Failed to write new binary: {}", e))?;

    // 设置可执行权限
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&new_path)
            .map_err(|e| format!("Failed to get metadata: {}", e))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&new_path, perms)
            .map_err(|e| format!("Failed to set permissions: {}", e))?;
    }

    // 备份当前二进制
    if current_exe.exists() {
        std::fs::copy(&current_exe, &backup_path)
            .map_err(|e| format!("Failed to backup current binary: {}", e))?;
    }

    // 替换二进制
    std::fs::rename(&new_path, &current_exe)
        .map_err(|e| format!("Failed to replace binary: {}", e))?;

    info!("Binary replaced successfully, scheduling restart...");
    *state.auto_update_state.update_progress.write().await = "restarting".to_string();

    // 调度重启（使用 systemd-run 延迟重启）
    let _ = Command::new("sudo")
        .args([
            "/usr/bin/systemd-run",
            "--no-block",
            "--on-active=2s",
            "/bin/systemctl",
            "restart",
            "xjp-deploy-agent",
        ])
        .output()
        .await;

    info!("Service restart scheduled for update");
    *state.auto_update_state.update_progress.write().await = "none".to_string();
    *state.auto_update_state.update_available.write().await = false;

    Ok(())
}

// ============== 自动更新功能结束 ==============

#[tokio::main]
async fn main() {
    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("xjp_deploy_agent=info".parse().unwrap()),
        )
        .json()
        .init();

    let state = Arc::new(AppState::new());

    info!(
        projects = ?state.projects.keys().collect::<Vec<_>>(),
        "XJP Deploy Agent starting"
    );

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/status", get(health_check))
        .route("/projects", get(list_projects))
        .route("/restart", post(restart_service))
        .route("/trigger/:project", post(trigger_deploy))
        .route("/tasks/:task_id", get(get_task_status))
        .route("/tasks/recent", get(get_recent_tasks))
        .route("/logs/:task_id/stream", get(stream_logs))
        // 系统信息端点
        .route("/system/info", get(get_system_info))
        .route("/system/stats", get(get_system_stats))
        // 容器管理端点
        .route("/containers", get(list_containers))
        .route("/containers/:name/logs", get(get_container_logs))
        .route("/containers/:name/env", get(get_container_env))
        .route("/containers/:name/env/full", get(get_container_env_full))
        // 数据库管理端点
        .route("/db/list", post(db_list_databases))
        .route("/db/tables", post(db_list_tables))
        .route("/db/schema", post(db_get_schema))
        .route("/db/query", post(db_execute_query))
        .route("/db/execute", post(db_execute_command))
        .route("/db/export", post(db_export_data))
        // 隧道端点
        .route("/tunnel/status", get(get_tunnel_status))
        .route("/tunnel/ws", get(tunnel_websocket_handler))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    // 如果是 Client 模式，启动隧道客户端
    if state.tunnel_mode == TunnelMode::Client {
        let client_state = state.clone();
        tokio::spawn(async move {
            start_tunnel_client(client_state).await;
        });
        info!("Tunnel client started");
    } else if state.tunnel_mode == TunnelMode::Server {
        info!("Tunnel server mode enabled, waiting for client connections on /tunnel/ws");
    }

    // 启动自动更新任务
    if state.auto_update_config.is_some() {
        let update_state = state.clone();
        tokio::spawn(async move {
            start_auto_update_task(update_state).await;
        });
        info!("Auto-update task started");
    }

    let port = env::var("PORT").unwrap_or_else(|_| "9876".to_string());
    let addr = format!("0.0.0.0:{}", port);

    info!(addr = %addr, "Listening");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
