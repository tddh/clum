#!/bin/bash
# 在目标 Linux 主机上部署 rmux-daemon
set -euo pipefail

REMOTE_HOST="${1:?Usage: $0 <user@host>}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "=== Deploying rmux-daemon to $REMOTE_HOST ==="

# 1. 安装 rmux（如果未安装或不完整）
#    官方安装脚本下载的是完整包（bin/rmux + bin/rmux-daemon + libexec/rmux/rmux private helper），
#    且安装后强制验证 helper 可达。
#    注意：不能只检测 `command -v rmux`——若主机只有手动拷贝的 bin 二进制（缺 libexec helper），
#    会被误判为"已安装"而跳过修复（表现为 term 连上即退 / `private rmux helper not found`）。
#    必须用 `rmux list-commands`（内部验证 helper 可达）作为完整性判据。
ssh "$REMOTE_HOST" 'RMUX_VERSION=0.10.0
# 完整性判据：rmux list-commands（内部验证 helper 可达）+ 版本匹配
if command -v rmux >/dev/null 2>&1 && rmux list-commands >/dev/null 2>&1 \
    && [ "$(rmux -V 2>/dev/null | awk "{print \$2}")" = "$RMUX_VERSION" ]; then
    echo "rmux ${RMUX_VERSION} already installed and complete"
else
    echo "Installing rmux ${RMUX_VERSION} (complete package: bin + daemon + libexec helper)..."
    # 显式装到系统前缀，确保 helper 与 bin 对齐到 /usr/local（与 rmux-bridge/systemd PATH 一致）
    RMUX_VERSION="v${RMUX_VERSION}" INSTALL_DIR=/usr/local/bin INSTALL_PREFIX=/usr/local \
        curl -fsSL https://rmux.io/install.sh | sh
    rmux list-commands >/dev/null 2>&1 || { echo "ERROR: rmux install incomplete (private helper missing)" >&2; exit 1; }
    [ "$(rmux -V 2>/dev/null | awk "{print \$2}")" = "$RMUX_VERSION" ] || { echo "ERROR: rmux version mismatch (expected ${RMUX_VERSION})" >&2; exit 1; }
fi'

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
