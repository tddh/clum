#!/bin/sh
set -e

# clum Bridge installer (Hub mode)
# Usage: curl -fsSLk -H "Authorization: Bearer <TOKEN>" https://SERVER:9788/releases/install.sh | \
#          BRIDGE_TOKEN=xxx SERVER_ADDR=10.0.0.1:9788 sh

if [ -z "${BRIDGE_TOKEN}" ]; then
    echo "ERROR: BRIDGE_TOKEN environment variable is required" >&2
    exit 1
fi

if [ -z "${SERVER_ADDR}" ]; then
    echo "ERROR: SERVER_ADDR environment variable is required" >&2
    exit 1
fi

ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  ARCH="x86_64" ;;
    aarch64) ARCH="aarch64" ;;
    *)       echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac

BASE_URL="https://${SERVER_ADDR}"

echo ">>> Installing clum bridge (${ARCH})"
echo ">>> Server: ${SERVER_ADDR}"

# Stop existing service to avoid "Text file busy"
if systemctl is-active rmux-bridge >/dev/null 2>&1; then
    echo ">>> Stopping existing rmux-bridge..."
    systemctl stop rmux-bridge
fi

# Migrate legacy layouts (idempotent). 迁移前先备份：与 deploy-bridge.sh 保持同样的
# 风险姿态，迁移出错时可以回退。两个目录必须使用**不同**的备份名——同一秒内
# 两次 `date +%s` 结果相同，同名 tar 会互相覆盖。
MIGRATION_STAMP=$(date +%s)
if [ -d /etc/yunying ] && [ ! -d /etc/clum ]; then
    echo ">>> Migrating /etc/yunying -> /etc/clum (backup first)"
    tar czf "/root/clum-migration-backup-${MIGRATION_STAMP}-etc.tar.gz" /etc/yunying
    mv /etc/yunying /etc/clum
fi
if [ -d /opt/agent-ops ] && [ ! -d /opt/clum ]; then
    echo ">>> Migrating /opt/agent-ops -> /opt/clum"
    tar czf "/root/clum-migration-backup-${MIGRATION_STAMP}-opt.tar.gz" /opt/agent-ops
    mv /opt/agent-ops /opt/clum
fi

# Clean up legacy files
rm -f /etc/clum/token
rm -rf /opt/yunying

mkdir -p /etc/clum /opt/clum/recordings

# Download binary (write to temp then move to avoid partial writes)
echo ">>> Downloading rmux-bridge..."
curl -fsSLk -H "Authorization: Bearer ${BRIDGE_TOKEN}" \
    "${BASE_URL}/releases/rmux-bridge-linux-${ARCH}" -o /tmp/rmux-bridge.download

# 首次安装无法验证服务端身份（自签 CA 的鸡蛋问题），但至少支持带外 checksum：
# 发布侧提供 sha256 后，运维通过 CLUM_RELEASE_SHA256 传入即可校验二进制；
# CA 证书同样提供 CLUM_CA_SHA256，避免二进制校验通过后被替换 CA。
if [ -n "${CLUM_RELEASE_SHA256:-}" ]; then
    echo ">>> Verifying binary checksum"
    echo "${CLUM_RELEASE_SHA256}  /tmp/rmux-bridge.download" | sha256sum -c - || {
        echo "ERROR: binary checksum mismatch" >&2
        rm -f /tmp/rmux-bridge.download
        exit 1
    }
else
    echo "WARNING: CLUM_RELEASE_SHA256 not set — binary integrity is NOT verified"
fi

chmod +x /tmp/rmux-bridge.download
mv -f /tmp/rmux-bridge.download /usr/local/bin/rmux-bridge

# Download CA cert
echo ">>> Downloading CA certificate..."
curl -fsSLk -H "Authorization: Bearer ${BRIDGE_TOKEN}" \
    "${BASE_URL}/releases/ca.crt" -o /etc/clum/ca.crt

if [ -n "${CLUM_CA_SHA256:-}" ]; then
    echo ">>> Verifying CA checksum"
    echo "${CLUM_CA_SHA256}  /etc/clum/ca.crt" | sha256sum -c - || {
        echo "ERROR: CA certificate checksum mismatch" >&2
        rm -f /etc/clum/ca.crt
        exit 1
    }
else
    echo "WARNING: CLUM_CA_SHA256 not set — CA certificate integrity is NOT verified"
fi

# Detect rmux socket (same logic as deploy-bridge.sh), fall back to standard path
RMUX_SOCKET=""
for d in /run/rmux "$HOME/.rmux" /tmp; do
    s=$(ls "$d"/rmux-*/default 2>/dev/null | head -1)
    if [ -n "$s" ]; then
        RMUX_SOCKET="$s"
        break
    fi
done
RMUX_SOCKET="${RMUX_SOCKET:-/root/.rmux/rmux-0/default}"
echo ">>> RMUX socket: ${RMUX_SOCKET}"

# Write bridge.env
cat > /etc/clum/bridge.env << EOF
BRIDGE_AUTH_TOKEN=${BRIDGE_TOKEN}
CLUM_SERVER_ADDR=${SERVER_ADDR}
CLUM_CA_CERT=/etc/clum/ca.crt
RECORDING_ENABLED=true
RECORDING_DIR=/opt/clum/recordings
BRIDGE_AUDIT_DB=/opt/clum/bridge_events.db
RMUX_SOCKET=${RMUX_SOCKET}
EOF
chmod 600 /etc/clum/bridge.env

# Install rmux daemon if not present, incomplete (missing libexec private helper), or version mismatch
# 仅检测 command -v rmux 会漏掉"只拷了 bin 二进制、缺 libexec/rmux/rmux helper"的残缺安装
# （表现为 term 连上即退 / private rmux helper not found），用 `rmux list-commands`
# （内部验证 helper 可达）作为完整性判据。官方脚本下载完整包并强制安装三件套。
RMUX_VERSION=0.10.0
if ! command -v rmux >/dev/null 2>&1 \
    || ! rmux list-commands >/dev/null 2>&1 \
    || [ "$(rmux -V 2>/dev/null | awk '{print $2}')" != "$RMUX_VERSION" ]; then
    echo ">>> Installing rmux ${RMUX_VERSION} (complete package: bin + daemon + libexec helper)..."
    RMUX_VERSION="v${RMUX_VERSION}" INSTALL_DIR=/usr/local/bin INSTALL_PREFIX=/usr/local \
        curl -fsSL https://rmux.io/install.sh | sh
    rmux list-commands >/dev/null 2>&1 || { echo "ERROR: rmux install incomplete (private helper missing)" >&2; exit 1; }
    [ "$(rmux -V 2>/dev/null | awk '{print $2}')" = "$RMUX_VERSION" ] || { echo "ERROR: rmux version mismatch" >&2; exit 1; }
fi

# Write systemd unit
cat > /etc/systemd/system/rmux-bridge.service << EOF
[Unit]
Description=clum Bridge
After=network.target rmux-daemon.service

[Service]
EnvironmentFile=/etc/clum/bridge.env
ExecStart=/usr/local/bin/rmux-bridge
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now rmux-bridge

echo ">>> Bridge installed and started."
echo ">>> Check: systemctl status rmux-bridge"
