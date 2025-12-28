//! FRP (frpc) process manager
//!
//! We manage `frpc` locally, generate a config from `TUNNEL_MAPPINGS`, and keep it running.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, RwLock},
    time::sleep,
};

use crate::{
    config::env::FrpConfig,
    domain::tunnel::{PortMapping, PortMappingStatus},
};

#[derive(Debug, Clone, Serialize)]
pub struct FrpcStatus {
    pub enabled: bool,
    pub desired_running: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<DateTime<Utc>>,
    pub restart_count: u32,
    pub last_error: Option<String>,
    pub config_path: String,
    pub server_addr: String,
    pub server_port: u16,
    pub mappings: Vec<PortMappingStatus>,
}

pub struct FrpcManager {
    config: FrpConfig,
    mappings: Vec<PortMapping>,
    desired_running: AtomicBool,
    child: Mutex<Option<Child>>,
    started_at: RwLock<Option<DateTime<Utc>>>,
    restart_count: AtomicU32,
    last_error: RwLock<Option<String>>,
    config_path: PathBuf,
    log_lines: Arc<RwLock<VecDeque<String>>>,
}

impl FrpcManager {
    pub fn new(config: FrpConfig, mappings: Vec<PortMapping>) -> Self {
        let config_path = std::env::temp_dir().join("xjp-deploy-agent-frpc.ini");
        Self {
            desired_running: AtomicBool::new(config.enabled),
            config,
            mappings,
            child: Mutex::new(None),
            started_at: RwLock::new(None),
            restart_count: AtomicU32::new(0),
            last_error: RwLock::new(None),
            config_path,
            log_lines: Arc::new(RwLock::new(VecDeque::new())),
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub async fn tail_logs(&self, tail: usize) -> Vec<String> {
        let tail = tail.clamp(1, 5000);
        let lines = self.log_lines.read().await;
        let start = lines.len().saturating_sub(tail);
        lines.iter().skip(start).cloned().collect()
    }

    fn build_ini(&self) -> String {
        let mut out = String::new();
        out.push_str("[common]\n");
        out.push_str(&format!("server_addr = {}\n", self.config.server_addr));
        out.push_str(&format!("server_port = {}\n", self.config.server_port));
        out.push_str("login_fail_exit = false\n");

        if let Some(ref token) = self.config.token {
            out.push_str(&format!("token = {}\n", token));
        }

        // Admin server (optional; useful for debugging)
        if !self.config.admin_addr.is_empty() && self.config.admin_port > 0 {
            out.push_str(&format!("admin_addr = {}\n", self.config.admin_addr));
            out.push_str(&format!("admin_port = {}\n", self.config.admin_port));
            if let Some(ref user) = self.config.admin_user {
                out.push_str(&format!("admin_user = {}\n", user));
            }
            if let Some(ref pwd) = self.config.admin_pwd {
                out.push_str(&format!("admin_pwd = {}\n", pwd));
            }
        }

        out.push('\n');

        for m in &self.mappings {
            // Use mapping.name as proxy section name; keep it deterministic.
            out.push_str(&format!("[{}]\n", m.name));
            out.push_str("type = tcp\n");
            out.push_str(&format!("local_ip = {}\n", m.local_host));
            out.push_str(&format!("local_port = {}\n", m.local_port));
            out.push_str(&format!("remote_port = {}\n", m.remote_port));
            out.push('\n');
        }

        out
    }

    async fn write_config(&self) -> Result<(), String> {
        let ini = self.build_ini();
        tokio::fs::write(&self.config_path, ini)
            .await
            .map_err(|e| format!("Failed to write frpc.ini: {}", e))
    }

    async fn process_state(&self) -> (bool, Option<u32>) {
        let mut guard = self.child.lock().await;
        let Some(child) = guard.as_mut() else {
            return (false, None);
        };

        match child.try_wait() {
            Ok(Some(status)) => {
                let msg = format!("frpc exited: {}", status);
                *self.last_error.write().await = Some(msg.clone());
                self.push_log(format!("[frp] {}", msg)).await;
                *guard = None;
                (false, None)
            }
            Ok(None) => (true, child.id()),
            Err(e) => {
                let msg = format!("Failed to query frpc status: {}", e);
                *self.last_error.write().await = Some(msg.clone());
                self.push_log(format!("[frp] {}", msg)).await;
                (true, child.id())
            }
        }
    }

    async fn push_log(&self, line: String) {
        const MAX_LINES: usize = 5000;
        let mut buf = self.log_lines.write().await;
        buf.push_back(line);
        while buf.len() > MAX_LINES {
            buf.pop_front();
        }
    }

    pub async fn start(&self) -> Result<(), String> {
        if !self.config.enabled {
            return Err("FRP is disabled (FRP_ENABLED=false)".to_string());
        }

        self.desired_running.store(true, Ordering::Relaxed);

        let (running, _) = self.process_state().await;
        if running {
            return Ok(());
        }

        self.write_config().await?;

        let mut guard = self.child.lock().await;
        if guard.is_some() {
            return Ok(());
        }

        self.push_log(format!(
            "[frp] starting frpc: {} -c {}",
            self.config.frpc_path,
            self.config_path.display()
        ))
        .await;

        let mut child = Command::new(&self.config.frpc_path)
            .arg("-c")
            .arg(self.config_path.to_string_lossy().to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to start frpc: {}", e))?;

        if let Some(stdout) = child.stdout.take() {
            let lines = self.log_lines.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let mut buf = lines.write().await;
                    buf.push_back(format!("[frpc stdout] {}", line));
                    while buf.len() > 5000 {
                        buf.pop_front();
                    }
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            let lines = self.log_lines.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let mut buf = lines.write().await;
                    buf.push_back(format!("[frpc stderr] {}", line));
                    while buf.len() > 5000 {
                        buf.pop_front();
                    }
                }
            });
        }

        *guard = Some(child);
        *self.started_at.write().await = Some(Utc::now());
        *self.last_error.write().await = None;

        Ok(())
    }

    pub async fn stop(&self) -> Result<(), String> {
        self.desired_running.store(false, Ordering::Relaxed);

        let mut guard = self.child.lock().await;
        let Some(mut child) = guard.take() else {
            return Ok(());
        };

        self.push_log("[frp] stopping frpc".to_string()).await;

        if let Err(e) = child.kill().await {
            let msg = format!("Failed to kill frpc: {}", e);
            *self.last_error.write().await = Some(msg.clone());
            self.push_log(format!("[frp] {}", msg)).await;
        }

        if let Err(e) = child.wait().await {
            let msg = format!("Failed to wait for frpc: {}", e);
            *self.last_error.write().await = Some(msg.clone());
            self.push_log(format!("[frp] {}", msg)).await;
        }

        Ok(())
    }

    pub async fn restart(&self) -> Result<(), String> {
        self.stop().await?;
        self.restart_count.fetch_add(1, Ordering::Relaxed);
        sleep(Duration::from_millis(800)).await;
        self.start().await
    }

    pub async fn status(&self) -> FrpcStatus {
        let desired_running = self.desired_running.load(Ordering::Relaxed);
        let (running, pid) = self.process_state().await;

        let mappings = self
            .mappings
            .iter()
            .map(|m| PortMappingStatus {
                mapping: m.clone(),
                status: if running {
                    "active".to_string()
                } else {
                    "disconnected".to_string()
                },
                active_connections: 0,
            })
            .collect();

        FrpcStatus {
            enabled: self.config.enabled,
            desired_running,
            running,
            pid,
            started_at: *self.started_at.read().await,
            restart_count: self.restart_count.load(Ordering::Relaxed),
            last_error: self.last_error.read().await.clone(),
            config_path: self.config_path.to_string_lossy().to_string(),
            server_addr: self.config.server_addr.clone(),
            server_port: self.config.server_port,
            mappings,
        }
    }

    /// Background loop: keep frpc alive while enabled.
    pub async fn supervise(self: Arc<Self>) {
        if !self.config.enabled {
            tracing::info!("FRP manager disabled");
            return;
        }

        tracing::info!(
            server_addr = %self.config.server_addr,
            server_port = self.config.server_port,
            mapping_count = self.mappings.len(),
            "FRP manager started"
        );

        if let Err(e) = self.start().await {
            tracing::warn!(error = %e, "Failed to start frpc");
        }

        loop {
            sleep(Duration::from_secs(2)).await;

            let desired = self.desired_running.load(Ordering::Relaxed);
            if !desired {
                continue;
            }

            let (running, _) = self.process_state().await;
            if running {
                continue;
            }

            self.restart_count.fetch_add(1, Ordering::Relaxed);
            self.push_log("[frp] frpc not running, restarting".to_string())
                .await;
            sleep(Duration::from_millis(1000)).await;
            if let Err(e) = self.start().await {
                tracing::warn!(error = %e, "Failed to restart frpc");
            }
        }
    }
}

