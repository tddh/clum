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

# Migrate legacy layouts (idempotent)
if [ -d /etc/yunying ] && [ ! -d /etc/clum ]; then
    echo ">>> Migrating /etc/yunying -> /etc/clum"
    mv /etc/yunying /etc/clum
fi
if [ -d /opt/agent-ops ] && [ ! -d /opt/clum ]; then
    echo ">>> Migrating /opt/agent-ops -> /opt/clum"
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
chmod +x /tmp/rmux-bridge.download
mv -f /tmp/rmux-bridge.download /usr/local/bin/rmux-bridge

# Download CA cert
echo ">>> Downloading CA certificate..."
curl -fsSLk -H "Authorization: Bearer ${BRIDGE_TOKEN}" \
    "${BASE_URL}/releases/ca.crt" -o /etc/clum/ca.crt

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

# Install rmux daemon if not present
if ! command -v rmux >/dev/null 2>&1; then
    echo ">>> Installing rmux daemon..."
    curl -fsSL https://rmux.io/install.sh | sh
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
