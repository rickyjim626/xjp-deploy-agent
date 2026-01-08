# SSH / 远程 Shell 功能设计方案

## 1. 背景

deploy-agent 已有的能力：
- `POST /exec` - 执行单个命令（需要 API Key）
- 隧道代理 - 可通过 GCP 服务器访问私有网络中的 Agent
- WebSocket 支持 - 隧道已使用 WebSocket

用户需求：
- 通过 deploy-agent 远程执行命令
- 支持交互式 shell 会话
- 无需在 Windows 上单独配置 SSH 服务器

## 2. 设计方案

### 2.1 架构图

```
┌─────────────────────────────────────────────────────────────────┐
│                         用户终端                                 │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────────────────┐  │
│  │ xjp shell   │  │ Web Terminal │  │ curl /exec            │  │
│  │ (CLI 工具)  │  │ (前端组件)   │  │ (单次命令)            │  │
│  └──────┬──────┘  └──────┬───────┘  └───────────┬───────────┘  │
└─────────┼────────────────┼──────────────────────┼───────────────┘
          │                │                      │
          │ WebSocket      │ WebSocket            │ HTTP
          ▼                ▼                      ▼
┌─────────────────────────────────────────────────────────────────┐
│                    GCP 服务器 (Tunnel Server)                    │
│                                                                  │
│  /tunnel/proxy/{client_id}/shell/ws  ──→  隧道转发              │
│  /tunnel/proxy/{client_id}/exec      ──→  隧道转发              │
└─────────────────────────────────────────────────────────────────┘
          │
          │ WebSocket Tunnel
          ▼
┌─────────────────────────────────────────────────────────────────┐
│                    远程 Agent (Windows/私有云)                   │
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │                    api/shell.rs                            │ │
│  │                                                            │ │
│  │  GET  /shell/ws     WebSocket 交互式 shell                 │ │
│  │  POST /exec         单次命令执行                            │ │
│  │  POST /exec/stream  流式命令执行 (SSE)                      │ │
│  └────────────────────────────────────────────────────────────┘ │
│                              │                                   │
│                              ▼                                   │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │                 services/shell.rs                          │ │
│  │                                                            │ │
│  │  ShellSession - 管理 PTY 会话                              │ │
│  │  - 创建伪终端 (PTY)                                        │ │
│  │  - 双向数据流 (stdin ↔ stdout/stderr)                      │ │
│  │  - 窗口大小调整                                            │ │
│  │  - 会话超时管理                                            │ │
│  └────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### 2.2 新增 API 端点

#### 2.2.1 WebSocket 交互式 Shell

```
GET /shell/ws
Headers: x-api-key: <API_KEY>
Query: ?cols=80&rows=24&shell=bash  (可选)

WebSocket 消息格式:
  Client → Server:
    { "type": "input", "data": "ls -la\n" }
    { "type": "resize", "cols": 120, "rows": 40 }
    { "type": "ping" }

  Server → Client:
    { "type": "output", "data": "..." }
    { "type": "error", "message": "..." }
    { "type": "exit", "code": 0 }
    { "type": "pong" }
```

#### 2.2.2 流式命令执行 (SSE)

```
POST /exec/stream
Headers: x-api-key: <API_KEY>
Body: {
  "command": "docker build -t myapp .",
  "cwd": "/app",
  "timeout": 300
}

Response: SSE Stream
  event: stdout
  data: {"line": "Step 1/10: FROM node:18"}

  event: stderr
  data: {"line": "Warning: ..."}

  event: exit
  data: {"code": 0, "duration_ms": 12345}
```

### 2.3 核心组件

#### 2.3.1 Shell 会话管理

```rust
// src/domain/shell.rs
pub struct ShellConfig {
    pub shell: String,           // "bash", "sh", "powershell", "cmd"
    pub cols: u16,               // 终端宽度
    pub rows: u16,               // 终端高度
    pub env: HashMap<String, String>,  // 额外环境变量
    pub cwd: Option<String>,     // 工作目录
    pub timeout_secs: u64,       // 会话超时 (默认 3600)
}

pub enum ShellMessage {
    Input(String),               // 用户输入
    Resize { cols: u16, rows: u16 },  // 窗口大小
    Ping,
}

pub enum ShellOutput {
    Output(String),              // 终端输出
    Error(String),               // 错误消息
    Exit(i32),                   // 退出码
    Pong,
}
```

#### 2.3.2 PTY 实现 (跨平台)

```rust
// src/services/shell.rs
pub struct ShellSession {
    pty: Box<dyn Pty>,           // 平台相关 PTY 实现
    child: Child,                // shell 进程
    timeout: Duration,
    created_at: Instant,
}

impl ShellSession {
    pub async fn new(config: ShellConfig) -> Result<Self, ShellError>;
    pub async fn write(&mut self, data: &[u8]) -> Result<(), ShellError>;
    pub async fn read(&mut self) -> Result<Vec<u8>, ShellError>;
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), ShellError>;
    pub fn is_alive(&self) -> bool;
    pub async fn close(self) -> i32;
}

// 平台适配
#[cfg(unix)]
mod unix {
    // 使用 portable-pty 或 pty crate
}

#[cfg(windows)]
mod windows {
    // 使用 ConPTY API (Windows 10+)
}
```

### 2.4 依赖库选择

```toml
# Cargo.toml 新增依赖

# 跨平台 PTY 支持
portable-pty = "0.8"        # 跨平台 PTY (Linux/macOS/Windows)

# 或者分别使用:
# [target.'cfg(unix)'.dependencies]
# pty = "0.2"               # Unix PTY
#
# [target.'cfg(windows)'.dependencies]
# conpty = "0.5"            # Windows ConPTY
```

### 2.5 安全考虑

1. **认证**: 所有 shell 端点必须验证 API Key
2. **授权**: 可选的命令白名单/黑名单
3. **审计**: 记录所有 shell 会话和命令
4. **超时**: 空闲会话自动断开
5. **限流**: 限制并发 shell 会话数量
6. **隔离**: 考虑使用沙箱或容器

```rust
// src/config/shell.rs
pub struct ShellSecurityConfig {
    pub enabled: bool,                    // 是否启用 shell 功能
    pub max_sessions: usize,              // 最大并发会话数 (默认 5)
    pub session_timeout_secs: u64,        // 会话超时 (默认 3600)
    pub idle_timeout_secs: u64,           // 空闲超时 (默认 300)
    pub allowed_shells: Vec<String>,      // 允许的 shell (默认 ["bash", "sh"])
    pub command_blacklist: Vec<String>,   // 禁止的命令模式
    pub audit_enabled: bool,              // 是否记录审计日志
}
```

## 3. 实现计划

### Phase 1: 基础功能 (1-2天)

1. 添加 `portable-pty` 依赖
2. 实现 `ShellSession` 核心逻辑
3. 添加 `/shell/ws` WebSocket 端点
4. 基本测试

### Phase 2: 增强功能 (1天)

1. 添加 `/exec/stream` SSE 端点
2. 窗口大小调整支持
3. 会话管理和超时

### Phase 3: 前端集成 (1天)

1. 部署中心添加 Web Terminal 组件
2. xjp CLI 添加 `shell` 子命令

### Phase 4: 安全加固 (1天)

1. 审计日志
2. 命令过滤
3. 限流和并发控制

## 4. 前端 Web Terminal

使用 xterm.js 实现浏览器内终端：

```tsx
// components/WebTerminal.tsx
import { Terminal } from 'xterm';
import { FitAddon } from 'xterm-addon-fit';
import { WebLinksAddon } from 'xterm-addon-web-links';

interface WebTerminalProps {
  agentUrl: string;
  apiKey: string;
}

export function WebTerminal({ agentUrl, apiKey }: WebTerminalProps) {
  // 1. 创建 Terminal 实例
  // 2. 连接 WebSocket
  // 3. 双向数据流
  // 4. 处理窗口大小变化
}
```

## 5. CLI 工具

```bash
# xjp CLI 新增 shell 命令

# 连接到指定 Agent 的 shell
xjp shell --agent privatecloud
xjp shell --agent windows
xjp shell --url https://auth.xiaojinpro.com/tunnel/proxy/windows

# 执行单个命令
xjp exec --agent windows "dir C:\\"
xjp exec --agent privatecloud "docker ps"

# 流式执行 (显示实时输出)
xjp exec --agent privatecloud --stream "docker build -t myapp ."
```

## 6. 配置示例

```env
# deploy-agent 环境变量

# 启用 shell 功能
SHELL_ENABLED=true
SHELL_DEFAULT=bash              # Windows 上可改为 powershell
SHELL_MAX_SESSIONS=5
SHELL_SESSION_TIMEOUT=3600
SHELL_IDLE_TIMEOUT=300
SHELL_AUDIT_ENABLED=true

# 安全设置
SHELL_ALLOWED_SHELLS=bash,sh,powershell
SHELL_COMMAND_BLACKLIST=rm -rf /,format,shutdown
```

## 7. 使用示例

### 7.1 通过 WebSocket 连接

```javascript
// 浏览器或 Node.js
const ws = new WebSocket('wss://auth.xiaojinpro.com/tunnel/proxy/windows/shell/ws', {
  headers: { 'x-api-key': 'your-api-key' }
});

ws.on('open', () => {
  ws.send(JSON.stringify({ type: 'input', data: 'dir\r\n' }));
});

ws.on('message', (data) => {
  const msg = JSON.parse(data);
  if (msg.type === 'output') {
    process.stdout.write(msg.data);
  }
});
```

### 7.2 通过 HTTP 执行命令

```bash
# 单次执行
curl -X POST https://auth.xiaojinpro.com/tunnel/proxy/windows/exec \
  -H "x-api-key: your-api-key" \
  -H "Content-Type: application/json" \
  -d '{"command": "dir C:\\", "timeout": 30}'

# 流式执行
curl -N https://auth.xiaojinpro.com/tunnel/proxy/windows/exec/stream \
  -H "x-api-key: your-api-key" \
  -H "Content-Type: application/json" \
  -d '{"command": "ping localhost -n 5"}'
```

## 8. 隧道代理扩展

现有的隧道 HTTP 代理已支持 WebSocket 升级，只需确保：

```rust
// api/tunnel.rs - 代理处理器需要支持 WebSocket 升级
async fn tunnel_proxy_handler(...) -> impl IntoResponse {
    // 检测 WebSocket 升级请求
    if is_websocket_upgrade(&headers) {
        // 建立 WebSocket 隧道
        return handle_websocket_proxy(...);
    }
    // 普通 HTTP 请求
    handle_http_proxy(...)
}
```

## 9. 总结

这个设计方案：
- ✅ 复用现有隧道基础设施
- ✅ 支持交互式 shell 和单次命令
- ✅ 跨平台 (Linux/macOS/Windows)
- ✅ 安全可控（认证、审计、超时）
- ✅ 前端可集成 Web Terminal
- ✅ CLI 工具可直接使用
