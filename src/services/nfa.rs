//! NFA (Neural Forced Aligner) supervisor + proxy
//!
//! Runs on Windows nodes (GPU/CPU) and exposes a stable HTTP API for backend usage.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, RwLock, Semaphore},
    time::{sleep, Instant},
};

use crate::config::env::NfaConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NfaAlignRequest {
    pub audio_base64: String,
    pub text: String,
    pub start_time: f64,
    pub frame_rate: i32,
    #[serde(default)]
    pub refine_boundaries: Option<bool>,
    #[serde(default)]
    pub fail_on_low_coverage: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NfaStatus {
    pub enabled: bool,
    pub desired_running: bool,
    pub running: bool,
    pub healthy: bool,
    pub pid: Option<u32>,
    pub port: u16,
    pub python_path: String,
    pub script_path: String,
    pub started_at: Option<DateTime<Utc>>,
    pub restart_count: u32,
    pub last_error: Option<String>,
}

struct NfaProcessState {
    child: Mutex<Option<Child>>,
    started_at: RwLock<Option<DateTime<Utc>>>,
    last_error: RwLock<Option<String>>,
    restart_count: AtomicU32,
}

impl NfaProcessState {
    fn new() -> Self {
        Self {
            child: Mutex::new(None),
            started_at: RwLock::new(None),
            last_error: RwLock::new(None),
            restart_count: AtomicU32::new(0),
        }
    }
}

/// Supervises `nfa_service.py` and proxies alignment requests.
pub struct NfaSupervisor {
    config: NfaConfig,
    desired_running: AtomicBool,
    process: NfaProcessState,
    semaphore: Semaphore,
    http: reqwest::Client,
    log_lines: Arc<RwLock<VecDeque<String>>>,
}

impl NfaSupervisor {
    pub fn new(config: NfaConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to build reqwest client");

        Self {
            desired_running: AtomicBool::new(config.auto_start),
            semaphore: Semaphore::new(config.max_concurrent.max(1)),
            process: NfaProcessState::new(),
            http,
            log_lines: Arc::new(RwLock::new(VecDeque::new())),
            config,
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn local_base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.config.port)
    }

    fn push_log_line(&self, line: String) {
        let log_lines = self.log_lines.clone();
        tokio::spawn(async move {
            const MAX_LINES: usize = 5000;
            let mut lines = log_lines.write().await;
            lines.push_back(line);
            while lines.len() > MAX_LINES {
                lines.pop_front();
            }
        });
    }

    pub async fn tail_logs(&self, tail: usize) -> Vec<String> {
        let tail = tail.clamp(1, 5000);
        let lines = self.log_lines.read().await;
        let start = lines.len().saturating_sub(tail);
        lines.iter().skip(start).cloned().collect()
    }

    pub async fn status(&self) -> NfaStatus {
        let desired_running = self.desired_running.load(Ordering::Relaxed);

        let (running, pid) = self.process_state().await;
        let healthy = self.health_check().await.unwrap_or(false);

        NfaStatus {
            enabled: self.config.enabled,
            desired_running,
            running,
            healthy,
            pid,
            port: self.config.port,
            python_path: self.config.python_path.clone(),
            script_path: self.config.script_path.clone(),
            started_at: *self.process.started_at.read().await,
            restart_count: self.process.restart_count.load(Ordering::Relaxed),
            last_error: self.process.last_error.read().await.clone(),
        }
    }

    async fn process_state(&self) -> (bool, Option<u32>) {
        let mut child_guard = self.process.child.lock().await;
        let Some(child) = child_guard.as_mut() else {
            return (false, None);
        };

        match child.try_wait() {
            Ok(Some(status)) => {
                let msg = format!("nfa_service exited: {}", status);
                *self.process.last_error.write().await = Some(msg.clone());
                self.push_log_line(format!("[supervisor] {}", msg));
                *child_guard = None;
                (false, None)
            }
            Ok(None) => (true, child.id()),
            Err(e) => {
                let msg = format!("Failed to query nfa_service status: {}", e);
                *self.process.last_error.write().await = Some(msg.clone());
                self.push_log_line(format!("[supervisor] {}", msg));
                (true, child.id())
            }
        }
    }

    async fn health_check(&self) -> Result<bool, String> {
        let url = format!("{}/health", self.local_base_url());
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("NFA health request failed: {}", e))?;
        Ok(resp.status().is_success())
    }

    pub async fn start(&self) -> Result<(), String> {
        if !self.config.enabled {
            return Err("NFA is disabled (NFA_ENABLED=false)".to_string());
        }

        self.desired_running.store(true, Ordering::Relaxed);

        // If already running, do nothing.
        let (running, _) = self.process_state().await;
        if running {
            return Ok(());
        }

        let mut child_guard = self.process.child.lock().await;
        if child_guard.is_some() {
            return Ok(());
        }

        let mut cmd = Command::new(&self.config.python_path);
        cmd.arg(&self.config.script_path)
            .arg("--port")
            .arg(self.config.port.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        self.push_log_line(format!(
            "[supervisor] starting nfa_service: {} {} --port {}",
            self.config.python_path, self.config.script_path, self.config.port
        ));

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to start nfa_service: {}", e))?;

        if let Some(stdout) = child.stdout.take() {
            let log_lines = self.log_lines.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut buf = log_lines.write().await;
                    buf.push_back(format!("[nfa stdout] {}", line));
                    while buf.len() > 5000 {
                        buf.pop_front();
                    }
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let log_lines = self.log_lines.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut buf = log_lines.write().await;
                    buf.push_back(format!("[nfa stderr] {}", line));
                    while buf.len() > 5000 {
                        buf.pop_front();
                    }
                }
            });
        }

        *child_guard = Some(child);
        *self.process.started_at.write().await = Some(Utc::now());
        *self.process.last_error.write().await = None;

        // Best-effort: wait for health to become ready.
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.health_check().await.unwrap_or(false) {
                self.push_log_line("[supervisor] nfa_service healthy".to_string());
                return Ok(());
            }
            sleep(Duration::from_millis(300)).await;
        }

        self.push_log_line("[supervisor] nfa_service started (health pending)".to_string());
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), String> {
        self.desired_running.store(false, Ordering::Relaxed);

        let mut child_guard = self.process.child.lock().await;
        let Some(mut child) = child_guard.take() else {
            return Ok(());
        };

        self.push_log_line("[supervisor] stopping nfa_service".to_string());

        if let Err(e) = child.kill().await {
            let msg = format!("Failed to kill nfa_service: {}", e);
            *self.process.last_error.write().await = Some(msg.clone());
            self.push_log_line(format!("[supervisor] {}", msg));
        }

        if let Err(e) = child.wait().await {
            let msg = format!("Failed to wait for nfa_service: {}", e);
            *self.process.last_error.write().await = Some(msg.clone());
            self.push_log_line(format!("[supervisor] {}", msg));
        }

        Ok(())
    }

    pub async fn restart(&self) -> Result<(), String> {
        self.stop().await?;
        sleep(Duration::from_millis(self.config.restart_backoff_ms)).await;
        self.process
            .restart_count
            .fetch_add(1, Ordering::Relaxed);
        self.start().await
    }

    async fn ensure_running(&self) -> Result<(), String> {
        if !self.config.enabled {
            return Err("NFA is disabled (NFA_ENABLED=false)".to_string());
        }

        if self.config.auto_start {
            return self.start().await;
        }

        let (running, _) = self.process_state().await;
        if running {
            Ok(())
        } else {
            Err("NFA is not running (NFA_AUTO_START=false)".to_string())
        }
    }

    async fn create_job(&self, req: &NfaAlignRequest) -> Result<String, String> {
        let url = format!("{}/align-nfa/job", self.local_base_url());
        let resp = self
            .http
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| format!("NFA create job failed: {}", e))?;

        let status = resp.status();
        let raw = resp
            .text()
            .await
            .unwrap_or_else(|_| "".to_string());

        if status == StatusCode::NOT_FOUND {
            return Err("NFA job endpoint not found".to_string());
        }
        if !status.is_success() {
            return Err(format!("NFA create job error {}: {}", status, raw));
        }

        let v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| format!("Invalid NFA create job JSON: {} ({})", e, raw))?;
        v.get("job_id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("Missing job_id in NFA response: {}", raw))
    }

    async fn poll_job(&self, job_id: &str) -> Result<serde_json::Value, String> {
        let url = format!("{}/align-nfa/job/{}", self.local_base_url(), job_id);
        let deadline = Instant::now() + Duration::from_secs(self.config.job_timeout_secs);

        while Instant::now() < deadline {
            let resp = self
                .http
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("NFA poll failed: {}", e))?;

            let status_code = resp.status();
            let raw = resp
                .text()
                .await
                .unwrap_or_else(|_| "".to_string());

            if !status_code.is_success() {
                return Err(format!("NFA poll error {}: {}", status_code, raw));
            }

            let v: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| format!("Invalid NFA poll JSON: {} ({})", e, raw))?;
            let status = v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown");

            match status {
                "done" => {
                    return Ok(v.get("result").cloned().unwrap_or(v));
                }
                "failed" => {
                    let err = v
                        .get("error")
                        .and_then(|x| x.as_str())
                        .unwrap_or("unknown error");
                    return Err(format!("NFA job failed: {}", err));
                }
                "pending" | "running" => {
                    sleep(Duration::from_millis(self.config.poll_interval_ms)).await;
                }
                other => {
                    return Err(format!("Unknown NFA job status: {}", other));
                }
            }
        }

        Err(format!(
            "NFA job timed out after {}s",
            self.config.job_timeout_secs
        ))
    }

    async fn align_sync(&self, req: &NfaAlignRequest) -> Result<serde_json::Value, String> {
        let url = format!("{}/align-nfa", self.local_base_url());
        let resp = self
            .http
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| format!("NFA sync align failed: {}", e))?;

        let status = resp.status();
        let raw = resp
            .text()
            .await
            .unwrap_or_else(|_| "".to_string());
        if !status.is_success() {
            return Err(format!("NFA sync align error {}: {}", status, raw));
        }

        serde_json::from_str(&raw)
            .map_err(|e| format!("Invalid NFA sync align JSON: {} ({})", e, raw))
    }

    /// Sync proxy API for backend: internally uses async jobs where possible.
    pub async fn align(&self, req: NfaAlignRequest) -> Result<serde_json::Value, String> {
        self.ensure_running().await?;

        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| "NFA semaphore closed".to_string())?;

        match self.create_job(&req).await {
            Ok(job_id) => self.poll_job(&job_id).await,
            Err(_) => self.align_sync(&req).await,
        }
    }

    /// Background supervision loop: restarts python process when it exits unexpectedly.
    pub async fn supervise(self: Arc<Self>) {
        if !self.config.enabled {
            tracing::info!("NFA supervisor disabled");
            return;
        }

        tracing::info!(
            port = self.config.port,
            auto_start = self.config.auto_start,
            "NFA supervisor started"
        );

        if self.config.auto_start {
            if let Err(e) = self.start().await {
                tracing::warn!(error = %e, "Failed to auto-start nfa_service");
            }
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

            self.process
                .restart_count
                .fetch_add(1, Ordering::Relaxed);
            let backoff = Duration::from_millis(self.config.restart_backoff_ms);
            self.push_log_line(format!(
                "[supervisor] nfa_service not running, restarting after {:?}",
                backoff
            ));
            sleep(backoff).await;

            if let Err(e) = self.start().await {
                tracing::warn!(error = %e, "Failed to restart nfa_service");
            }
        }
    }
}
