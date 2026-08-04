#!/bin/bash
# 在目标 Linux 主机上部署 rmux-daemon
set -euo pipefail

REMOTE_HOST="${1:?Usage: $0 <user@host>}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "=== Deploying rmux-daemon to $REMOTE_HOST ==="

# 1. 安装 rmux（如果未安装）
ssh "$REMOTE_HOST" 'command -v rmux || curl -fsSL https://rmux.io/install.sh | sh'

# 2. 上传 rmux-daemon.service
scp "$SCRIPT_DIR/rmux-daemon.service" "$REMOTE_HOST:/tmp/rmux-daemon.service"
ssh "$REMOTE_HOST" "sudo mv /tmp/rmux-daemon.service /etc/systemd/system/rmux-daemon.service"

# 3. 启动 daemon
ssh "$REMOTE_HOST" "sudo systemctl daemon-reload && sudo systemctl enable --now rmux-daemon"
echo "rmux-daemon started"

# 4. 写入 profile.d，方便用户直接使用 rmux CLI
ssh "$REMOTE_HOST" "echo 'export RMUX_TMPDIR=\$HOME/.rmux' | sudo tee /etc/profile.d/clum.sh > /dev/null"
echo "Wrote RMUX_TMPDIR=\$HOME/.rmux to /etc/profile.d/clum.sh"

echo ""
echo "=== Daemon deployment complete ==="
echo "Users can now run: rmux a -t clum"
