#!/bin/bash
# 在目标 Linux 主机上部署 rmux-bridge
# 前置条件：rmux-daemon 已安装并运行
set -euo pipefail

BRIDGE_BINARY="${1:?Usage: $0 <bridge-binary> <user@host> [<certs-dir>]}"
REMOTE_HOST="${2:?Usage: $0 <bridge-binary> <user@host> [<certs-dir>]}"
CERTS_DIR="${3:-certs}"
REMOTE_DIR="/opt/clum"
BRIDGE_TOKEN="${BRIDGE_TOKEN:-$(openssl rand -hex 32)}"

HOST_IP=$(echo "$REMOTE_HOST" | cut -d@ -f2 | cut -d: -f1)
HOST_CERT="$CERTS_DIR/${HOST_IP}.crt"
HOST_KEY="$CERTS_DIR/${HOST_IP}.key"

echo "=== Deploying rmux-bridge to $REMOTE_HOST ==="

# 0. 检查证书
if [ ! -f "$HOST_CERT" ] || [ ! -f "$HOST_KEY" ]; then
    echo "ERROR: Certificate not found for $HOST_IP"
    echo "  Run: bash deploy/generate-certs.sh $CERTS_DIR $HOST_IP"
    echo "  Then re-run deploy."
    exit 1
fi

# 1. 安装 rmux（如果未安装）
ssh "$REMOTE_HOST" 'command -v rmux || curl -fsSL https://rmux.io/install.sh | sh'

# 2. 写入 profile.d，方便用户直接使用 rmux CLI（与 rmux-daemon.service 的 RMUX_TMPDIR 保持一致）
ssh "$REMOTE_HOST" "echo 'export RMUX_TMPDIR=\$HOME/.rmux' | sudo tee /etc/profile.d/clum.sh > /dev/null"
echo "Wrote RMUX_TMPDIR=\$HOME/.rmux to /etc/profile.d/clum.sh"

# 3. 创建目录
ssh "$REMOTE_HOST" "sudo mkdir -p $REMOTE_DIR/certs && sudo chown \$USER:\$USER $REMOTE_DIR"

# 4. 上传 bridge 二进制
scp "$BRIDGE_BINARY" "$REMOTE_HOST:$REMOTE_DIR/"
ssh "$REMOTE_HOST" "sudo chmod 755 $REMOTE_DIR/rmux-bridge"

# 5. 上传主机专属的 TLS 证书
scp "$HOST_CERT" "$REMOTE_HOST:$REMOTE_DIR/certs/${HOST_IP}.crt"
scp "$HOST_KEY" "$REMOTE_HOST:$REMOTE_DIR/certs/${HOST_IP}.key"
ssh "$REMOTE_HOST" "chmod 600 $REMOTE_DIR/certs/${HOST_IP}.key"

# 6. 写入 token
ssh "$REMOTE_HOST" "echo 'BRIDGE_AUTH_TOKEN=$BRIDGE_TOKEN' | sudo tee $REMOTE_DIR/bridge.env > /dev/null && sudo chmod 600 $REMOTE_DIR/bridge.env"

# 7. 检测 rmux socket 路径（优先 /run/rmux，然后 \$HOME/.rmux，回退 /tmp）
RMUX_SOCK=$(ssh "$REMOTE_HOST" "for d in /run/rmux \$HOME/.rmux /tmp; do s=\$(ls \$d/rmux-*/default 2>/dev/null | head -1); [ -n \"\$s\" ] && echo \$s && break; done; [ -z \"\$s\" ] && echo '/tmp/rmux-0/default'")
echo "Detected rmux socket: $RMUX_SOCK"

# 8. 创建 rmux-bridge.service
ssh "$REMOTE_HOST" "sudo tee /etc/systemd/system/rmux-bridge.service" <<SERVICE_EOF
[Unit]
Description=RMUX Bridge - QUIC to Unix socket proxy
After=network.target rmux-daemon.service
Requires=rmux-daemon.service

[Service]
Type=simple
EnvironmentFile=/opt/clum/bridge.env
ExecStart=/opt/clum/rmux-bridge \\
    --quic-listen-addr 0.0.0.0:9778 \\
    --max-connections 256 \\
    --rmux-socket $RMUX_SOCK \\
    --tls-cert /opt/clum/certs/${HOST_IP}.crt \\
    --tls-key /opt/clum/certs/${HOST_IP}.key
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
SERVICE_EOF

# 9. 启动 bridge 服务
ssh "$REMOTE_HOST" "sudo systemctl daemon-reload && sudo systemctl enable --now rmux-bridge"

echo ""
echo "=== Deployment complete ==="
echo "Host:     $REMOTE_HOST"
echo "Token:    $BRIDGE_TOKEN"
echo "MCP --ca-cert:  $CERTS_DIR/ca.crt"
echo ""
echo "Add this to config/hosts.yaml:"
echo ""
echo "  - name: $(echo "$REMOTE_HOST" | cut -d@ -f2 | cut -d. -f1)"
echo "    bridge_addr: $(echo "$REMOTE_HOST" | cut -d@ -f2):9778"
echo "    bridge_token: \"$BRIDGE_TOKEN\""
echo "    tags: []"
