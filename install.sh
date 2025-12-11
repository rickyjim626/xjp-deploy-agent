#!/bin/bash
# XJP Deploy Agent 安装脚本
# 在私有云服务器上运行此脚本

set -e

INSTALL_DIR="/opt/xjp-deploy-agent"
SERVICE_NAME="xjp-deploy-agent"

echo "=== XJP Deploy Agent 安装脚本 ==="

# 检查是否为 root
if [ "$EUID" -ne 0 ]; then
    echo "请使用 root 权限运行此脚本"
    exit 1
fi

# 创建安装目录
mkdir -p $INSTALL_DIR

# 检查是否有预编译的二进制文件
if [ -f "./target/release/xjp-deploy-agent" ]; then
    echo "复制二进制文件..."
    cp ./target/release/xjp-deploy-agent $INSTALL_DIR/
elif [ -f "./xjp-deploy-agent" ]; then
    echo "复制二进制文件..."
    cp ./xjp-deploy-agent $INSTALL_DIR/
else
    echo "未找到二进制文件，请先编译: cargo build --release"
    exit 1
fi

chmod +x $INSTALL_DIR/xjp-deploy-agent

# 生成 API 密钥（如果不存在）
if [ ! -f "$INSTALL_DIR/.env" ]; then
    API_KEY=$(openssl rand -hex 32)
    cat > $INSTALL_DIR/.env << EOF
# XJP Deploy Agent 配置

# API 密钥（用于验证请求）
DEPLOY_AGENT_API_KEY=$API_KEY

# 监听端口
PORT=9876

# 日志级别
RUST_LOG=info

# 回调 URL（部署完成后通知部署中心）
DEPLOY_CENTER_CALLBACK_URL=https://auth.xiaojinpro.com

# 项目配置
# xiaojinpro-backend 默认已配置
BACKEND_WORK_DIR=/root/xiaojinpro-backend

# 添加更多项目示例:
# PROJECT_FRONTEND_DIR=/root/xiaojinpro-frontend
# PROJECT_FRONTEND_SCRIPT=./deploy.sh
EOF
    echo ""
    echo "已生成配置文件: $INSTALL_DIR/.env"
    echo "API 密钥: $API_KEY"
    echo "请妥善保管此密钥！"
fi

# 创建 systemd 服务
cat > /etc/systemd/system/$SERVICE_NAME.service << EOF
[Unit]
Description=XJP Deploy Agent
After=network.target

[Service]
Type=simple
User=root
WorkingDirectory=$INSTALL_DIR
EnvironmentFile=$INSTALL_DIR/.env
ExecStart=$INSTALL_DIR/xjp-deploy-agent
Restart=always
RestartSec=5

# 日志
StandardOutput=journal
StandardError=journal
SyslogIdentifier=$SERVICE_NAME

[Install]
WantedBy=multi-user.target
EOF

# 重载 systemd
systemctl daemon-reload

echo ""
echo "=== 安装完成 ==="
echo ""
echo "启动服务:    systemctl start $SERVICE_NAME"
echo "停止服务:    systemctl stop $SERVICE_NAME"
echo "查看状态:    systemctl status $SERVICE_NAME"
echo "查看日志:    journalctl -u $SERVICE_NAME -f"
echo "开机自启:    systemctl enable $SERVICE_NAME"
echo ""
echo "配置文件:    $INSTALL_DIR/.env"
echo "API 端点:    http://localhost:9876"
echo ""
echo "测试:        curl http://localhost:9876/health"
