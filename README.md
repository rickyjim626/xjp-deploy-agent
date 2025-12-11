# XJP Deploy Agent

私有云部署代理 - 用于触发构建并实时流式传输日志。

## 功能

- 接收部署触发请求
- 执行本地构建脚本
- 实时流式传输构建日志到 deploy-center
- 支持多项目配置
- 自动取消旧构建（新提交到达时）
- 健康检查和版本信息

## API 端点

- `GET /health` - 健康检查，返回版本、项目列表、活跃任务
- `GET /status` - 同 `/health`
- `GET /projects` - 列出配置的项目
- `POST /trigger/:project` - 触发指定项目的部署
- `GET /tasks/:task_id` - 获取任务状态
- `GET /logs/:task_id/stream` - SSE 流式获取构建日志

## 配置

通过环境变量配置：

```bash
# 服务端口
PORT=9877

# API 密钥（用于验证请求）
API_KEY=your-api-key

# 回调 URL（部署完成后通知 deploy-center）
CALLBACK_URL=https://auth.xiaojinpro.com/api/deploy

# 项目配置（支持多个项目）
PROJECT_MYPROJECT_DIR=/path/to/project
PROJECT_MYPROJECT_SCRIPT=/path/to/deploy-script.sh
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

## License

MIT
