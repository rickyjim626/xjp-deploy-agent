//! SSH 服务器实现

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use russh::server::{Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, CryptoVec, MethodSet};
use russh_keys::key::KeyPair;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::domain::ssh::{SshAuthMode, SshConfig, SshHealthInfo, SshSessionInfo, SshServerStatus};

/// SSH 服务器
pub struct SshServer {
    config: SshConfig,
    api_key: String,
    host_key: Arc<KeyPair>,
    sessions: Arc<RwLock<HashMap<String, SshSessionInfo>>>,
}

impl SshServer {
    /// 创建新的 SSH 服务器
    pub async fn new(config: SshConfig, api_key: String) -> anyhow::Result<Self> {
        // 生成或加载主机密钥
        let host_key = if let Some(ref key_file) = config.host_key_file {
            if std::path::Path::new(key_file).exists() {
                info!(key_file = %key_file, "Loading SSH host key from file");
                let key_data = tokio::fs::read(key_file).await?;
                russh_keys::decode_secret_key(&String::from_utf8(key_data)?, None)?
            } else {
                info!(key_file = %key_file, "Generating new SSH host key");
                let key = KeyPair::generate_ed25519();
                // 保存密钥 - 使用 PKCS8 PEM 格式
                let mut key_bytes = Vec::new();
                russh_keys::encode_pkcs8_pem(&key, &mut key_bytes)?;
                tokio::fs::write(key_file, &key_bytes).await?;
                key
            }
        } else {
            info!("Generating ephemeral SSH host key");
            KeyPair::generate_ed25519()
        };

        Ok(Self {
            config,
            api_key,
            host_key: Arc::new(host_key),
            sessions: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// 启动 SSH 服务器
    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let addr = format!("0.0.0.0:{}", self.config.port);
        info!(addr = %addr, "Starting SSH server");

        let russh_config = russh::server::Config {
            inactivity_timeout: Some(Duration::from_secs(self.config.idle_timeout_secs)),
            auth_rejection_time: Duration::from_secs(1),
            auth_rejection_time_initial: Some(Duration::from_secs(0)),
            keys: vec![(*self.host_key).clone()],
            ..Default::default()
        };

        let russh_config = Arc::new(russh_config);

        // 绑定 TCP 监听器
        let listener = TcpListener::bind(&addr).await?;

        // 创建一个可变的 server wrapper
        let mut server = SshServerWrapper {
            inner: self,
        };

        server.run_on_socket(russh_config, &listener).await?;

        Ok(())
    }

    /// 获取服务器状态
    pub async fn status(&self) -> SshServerStatus {
        let sessions = self.sessions.read().await;
        SshServerStatus {
            enabled: self.config.enabled,
            listening: true,
            port: self.config.port,
            active_sessions: sessions.len(),
            max_sessions: self.config.max_sessions,
            sessions: sessions.values().cloned().collect(),
        }
    }

    /// 获取健康信息
    pub async fn health_info(&self) -> SshHealthInfo {
        let sessions = self.sessions.read().await;
        SshHealthInfo {
            enabled: self.config.enabled,
            port: self.config.port,
            active_sessions: sessions.len(),
        }
    }
}

/// SSH 服务器包装器，实现 Server trait
struct SshServerWrapper {
    inner: Arc<SshServer>,
}

impl Server for SshServerWrapper {
    type Handler = SshSessionHandler;

    fn new_client(&mut self, peer_addr: Option<std::net::SocketAddr>) -> Self::Handler {
        let session_id = uuid::Uuid::new_v4().to_string();
        let client_addr = peer_addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        info!(session_id = %session_id, client_addr = %client_addr, "New SSH client connection");

        SshSessionHandler {
            session_id,
            client_addr,
            username: None,
            authenticated: false,
            api_key: self.inner.api_key.clone(),
            ssh_username: self.inner.config.username.clone(),
            auth_mode: self.inner.config.auth_mode.clone(),
            sessions: self.inner.sessions.clone(),
            channels: HashMap::new(),
        }
    }
}

/// SSH 会话处理器
pub struct SshSessionHandler {
    session_id: String,
    client_addr: String,
    username: Option<String>,
    authenticated: bool,
    api_key: String,
    ssh_username: String,
    auth_mode: SshAuthMode,
    sessions: Arc<RwLock<HashMap<String, SshSessionInfo>>>,
    channels: HashMap<ChannelId, ChannelState>,
}

/// 通道状态
struct ChannelState {
    /// 是否是 PTY 会话
    #[allow(dead_code)]
    pty: bool,
    /// 环境变量
    env: HashMap<String, String>,
    /// 子进程
    child: Option<tokio::process::Child>,
}

#[async_trait]
impl Handler for SshSessionHandler {
    type Error = anyhow::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        debug!(session_id = %self.session_id, user = %user, "SSH password auth attempt");

        // 检查认证模式
        if matches!(self.auth_mode, SshAuthMode::PublicKey) {
            return Ok(Auth::Reject {
                proceed_with_methods: Some(MethodSet::PUBLICKEY),
            });
        }

        if user == self.ssh_username && password == self.api_key {
            info!(session_id = %self.session_id, user = %user, "SSH password auth success");
            self.username = Some(user.to_string());
            self.authenticated = true;

            // 记录会话
            let session_info = SshSessionInfo {
                session_id: self.session_id.clone(),
                username: user.to_string(),
                client_addr: self.client_addr.clone(),
                connected_at: chrono::Utc::now(),
                last_activity: chrono::Utc::now(),
            };
            self.sessions.write().await.insert(self.session_id.clone(), session_info);

            Ok(Auth::Accept)
        } else {
            warn!(session_id = %self.session_id, user = %user, "SSH password auth failed");
            Ok(Auth::Reject {
                proceed_with_methods: None,
            })
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &russh_keys::key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        debug!(
            session_id = %self.session_id,
            user = %user,
            key_type = ?public_key.name(),
            "SSH public key auth attempt"
        );

        // TODO: 实现公钥认证 (从 authorized_keys 文件读取)
        // 目前只支持密码认证

        if matches!(self.auth_mode, SshAuthMode::ApiKey) {
            return Ok(Auth::Reject {
                proceed_with_methods: Some(MethodSet::PASSWORD),
            });
        }

        Ok(Auth::Reject {
            proceed_with_methods: None,
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        if !self.authenticated {
            return Ok(false);
        }

        debug!(session_id = %self.session_id, channel_id = ?channel.id(), "Channel open session");

        self.channels.insert(
            channel.id(),
            ChannelState {
                pty: false,
                env: HashMap::new(),
                child: None,
            },
        );

        Ok(true)
    }

    async fn pty_request(
        &mut self,
        channel_id: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(
            session_id = %self.session_id,
            channel_id = ?channel_id,
            term = %term,
            cols = col_width,
            rows = row_height,
            "PTY request"
        );

        if let Some(state) = self.channels.get_mut(&channel_id) {
            state.pty = true;
            state.env.insert("TERM".to_string(), term.to_string());
            state.env.insert("COLUMNS".to_string(), col_width.to_string());
            state.env.insert("LINES".to_string(), row_height.to_string());
        }

        session.channel_success(channel_id);
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel_id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(session_id = %self.session_id, channel_id = ?channel_id, "Shell request");

        if !self.authenticated {
            session.channel_failure(channel_id);
            return Ok(());
        }

        // 启动 shell
        let shell = if cfg!(windows) {
            "powershell.exe"
        } else {
            "/bin/bash"
        };

        let mut cmd = tokio::process::Command::new(shell);

        // 设置环境变量
        if let Some(state) = self.channels.get(&channel_id) {
            for (key, value) in &state.env {
                cmd.env(key, value);
            }
        }

        // 配置进程
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        match cmd.spawn() {
            Ok(child) => {
                if let Some(state) = self.channels.get_mut(&channel_id) {
                    state.child = Some(child);
                }
                session.channel_success(channel_id);

                // TODO: 启动 I/O 转发任务
                // 这需要更复杂的架构来处理 stdout/stderr 的异步读取
            }
            Err(e) => {
                error!(error = %e, "Failed to spawn shell");
                session.channel_failure(channel_id);
            }
        }

        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data);
        debug!(session_id = %self.session_id, channel_id = ?channel_id, command = %command, "Exec request");

        if !self.authenticated {
            session.channel_failure(channel_id);
            return Ok(());
        }

        // 执行命令
        let (shell, args) = if cfg!(windows) {
            ("powershell.exe", vec!["-Command", &command])
        } else {
            ("sh", vec!["-c", &command])
        };

        let mut cmd = tokio::process::Command::new(shell);
        cmd.args(&args);

        // 设置环境变量
        if let Some(state) = self.channels.get(&channel_id) {
            for (key, value) in &state.env {
                cmd.env(key, value);
            }
        }

        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        match cmd.output().await {
            Ok(output) => {
                session.channel_success(channel_id);

                // 发送 stdout
                if !output.stdout.is_empty() {
                    session.data(channel_id, CryptoVec::from(output.stdout));
                }

                // 发送 stderr
                if !output.stderr.is_empty() {
                    session.extended_data(channel_id, 1, CryptoVec::from(output.stderr));
                }

                // 发送退出状态
                let exit_code = output.status.code().unwrap_or(-1) as u32;
                session.exit_status_request(channel_id, exit_code);
                session.close(channel_id);
            }
            Err(e) => {
                error!(error = %e, "Failed to execute command");
                let error_msg = format!("Failed to execute command: {}\n", e);
                session.extended_data(channel_id, 1, CryptoVec::from(error_msg.into_bytes()));
                session.exit_status_request(channel_id, 1);
                session.close(channel_id);
            }
        }

        Ok(())
    }

    async fn env_request(
        &mut self,
        channel_id: ChannelId,
        variable_name: &str,
        variable_value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(
            session_id = %self.session_id,
            channel_id = ?channel_id,
            name = %variable_name,
            value = %variable_value,
            "Env request"
        );

        if let Some(state) = self.channels.get_mut(&channel_id) {
            state.env.insert(variable_name.to_string(), variable_value.to_string());
        }

        session.channel_success(channel_id);
        Ok(())
    }

    async fn data(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 将数据转发到子进程的 stdin
        if let Some(state) = self.channels.get_mut(&channel_id) {
            if let Some(ref mut child) = state.child {
                if let Some(ref mut stdin) = child.stdin {
                    use tokio::io::AsyncWriteExt;
                    if let Err(e) = stdin.write_all(data).await {
                        error!(error = %e, "Failed to write to child stdin");
                    }
                }
            }
        }

        // 更新最后活动时间
        if let Some(info) = self.sessions.write().await.get_mut(&self.session_id) {
            info.last_activity = chrono::Utc::now();
        }

        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel_id: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(session_id = %self.session_id, channel_id = ?channel_id, "Channel close");

        // 终止子进程
        if let Some(mut state) = self.channels.remove(&channel_id) {
            if let Some(mut child) = state.child.take() {
                let _ = child.kill().await;
            }
        }

        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel_id: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(session_id = %self.session_id, channel_id = ?channel_id, "Channel EOF");

        // 关闭子进程的 stdin
        if let Some(state) = self.channels.get_mut(&channel_id) {
            if let Some(ref mut child) = state.child {
                child.stdin.take();
            }
        }

        Ok(())
    }
}

impl Drop for SshSessionHandler {
    fn drop(&mut self) {
        let session_id = self.session_id.clone();
        let sessions = self.sessions.clone();

        tokio::spawn(async move {
            sessions.write().await.remove(&session_id);
            info!(session_id = %session_id, "SSH session ended");
        });
    }
}
