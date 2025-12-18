# XJP Deploy Agent

私有云部署代理 - 用于触发构建并实时流式传输日志。

## 功能

- 接收部署触发请求
- 执行本地构建脚本
- 实时流式传输构建日志到 deploy-center
- 支持多项目配置
- 自动取消旧构建（新提交到达时）
- 健康检查和版本信息
- 隧道功能（FRP 风格的端口转发）
- 自动更新

## API 端点

- `GET /health` - 健康检查，返回版本、项目列表、活跃任务
- `GET /status` - 同 `/health`
- `GET /projects` - 列出配置的项目
- `POST /trigger/:project` - 触发指定项目的部署
- `GET /tasks/:task_id` - 获取任务状态（活跃任务和历史记录）
- `GET /tasks/recent` - 获取最近的任务历史
- `GET /logs/:task_id/stream` - SSE 流式获取构建日志
- `GET /system/info` - 获取系统信息
- `GET /system/stats` - 获取系统负载统计
- `GET /containers` - 列出 Docker 容器
- `GET /tunnel/status` - 获取隧道状态

## 配置

通过环境变量配置：

### 核心配置

```bash
# 服务端口（默认 9877）
PORT=9877

# API 密钥（用于验证请求）
API_KEY=your-api-key

# 部署中心回调 URL
# 推荐使用新名称:
DEPLOY_CENTER_URL=https://auth.xiaojinpro.com
# 兼容旧名称 (deprecated):
CALLBACK_URL=https://auth.xiaojinpro.com
```

### 项目配置

支持多种部署类型：

#### Script 类型（执行脚本）

```bash
PROJECT_MYPROJECT_DIR=/path/to/project
PROJECT_MYPROJECT_TYPE=script
PROJECT_MYPROJECT_SCRIPT=./deploy.sh
```

#### Docker Compose 类型

```bash
PROJECT_MYSERVICE_DIR=/path/to/compose
PROJECT_MYSERVICE_TYPE=docker_compose
PROJECT_MYSERVICE_IMAGE=ghcr.io/user/image:latest
PROJECT_MYSERVICE_COMPOSE_FILE=docker-compose.yml
PROJECT_MYSERVICE_GIT_REPO=/path/to/git/repo  # 可选，用于 git pull
PROJECT_MYSERVICE_SERVICE=myservice           # 可选，指定重启的服务
```

#### Docker Build 类型

```bash
PROJECT_MYAPP_DIR=/path/to/source
PROJECT_MYAPP_TYPE=docker_build
PROJECT_MYAPP_IMAGE=ghcr.io/user/myapp
PROJECT_MYAPP_DOCKERFILE=Dockerfile           # 可选，默认 Dockerfile
PROJECT_MYAPP_BUILD_CONTEXT=.                 # 可选，默认 .
PROJECT_MYAPP_TAG=latest                      # 可选，默认使用 git commit hash
PROJECT_MYAPP_PUSH_LATEST=true                # 可选，是否同时推送 :latest tag
PROJECT_MYAPP_BRANCH=master                   # 可选，默认 master
```

#### Xiaojincut 类型

```bash
PROJECT_XJC_DIR=/path/to/work
PROJECT_XJC_TYPE=xiaojincut
PROJECT_XJC_TASK_TYPE=import                  # import, export, status, timeline
PROJECT_XJC_INPUT_PATH=/path/to/input         # 可选
PROJECT_XJC_OUTPUT_DIR=/path/to/output        # 可选
PROJECT_XJC_PROMPT="AI prompt"                # 可选
PROJECT_XJC_PORT=19527                        # 可选，默认 19527
```

### 隧道配置

```bash
# 隧道模式: server, client, disabled (默认 disabled)
TUNNEL_MODE=client

# 隧道认证令牌
TUNNEL_AUTH_TOKEN=your-tunnel-token

# 客户端模式：服务端 URL
TUNNEL_SERVER_URL=ws://your-server:9877/tunnel

# 客户端模式：端口映射
# 格式: name:remote_port:local_host:local_port,...
TUNNEL_MAPPINGS=minio:19000:localhost:9000,postgres:15432:localhost:5432
```

### 自动更新配置

```bash
# 自动更新端点
AUTO_UPDATE_ENDPOINT=https://releases.example.com/xjp-deploy-agent

# 检查间隔（秒，默认 3600）
AUTO_UPDATE_INTERVAL=3600
```

### 遗留配置（已废弃）

以下环境变量仍然支持但已废弃，建议迁移到新格式：

```bash
# 已废弃，使用 PROJECT_*_DIR 替代
BACKEND_WORK_DIR=/path/to/backend

# 已废弃，使用 DEPLOY_CENTER_URL 替代
CALLBACK_URL=https://auth.xiaojinpro.com
```

## 安装

### 使用安装脚本

```bash
./install.sh
```

### 手动安装

1. 编译：
```bash
cargo build --release
```

2. 复制二进制文件：
```bash
sudo cp target/release/xjp-deploy-agent /opt/xjp-deploy-agent/
```

3. 创建 systemd 服务：
```bash
sudo cp xjp-deploy-agent.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable xjp-deploy-agent
sudo systemctl start xjp-deploy-agent
```

## 开发

```bash
# 运行开发服务器
cargo run

# 运行测试
cargo test

# 格式化代码
cargo fmt

# 检查代码
cargo clippy
```

## 架构

重构后的模块结构：

```
src/
├── api/           # HTTP 处理器
├── config/        # 配置加载
├── domain/        # 纯领域模型（无 tokio 依赖）
├── infra/         # 基础设施（HTTP 客户端等）
├── middleware/    # Axum 中间件
├── services/      # 业务逻辑
├── state/         # 运行时状态（含 tokio 同步原语）
├── error.rs       # 统一错误处理
└── lib.rs         # 库入口
```

## License

MIT
