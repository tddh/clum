#!/bin/sh
set -e

# yunying Bridge installer (Hub mode)
# Usage: curl -fsSLk -H "Authorization: Bearer <TOKEN>" https://SERVER:9788/install.sh | \
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
AUTH="-H \"Authorization: Bearer ${BRIDGE_TOKEN}\""

echo ">>> Installing yunying bridge (${ARCH})"
echo ">>> Server: ${SERVER_ADDR}"

# Stop existing service to avoid "Text file busy"
if systemctl is-active rmux-bridge >/dev/null 2>&1; then
    echo ">>> Stopping existing rmux-bridge..."
    systemctl stop rmux-bridge
fi

# Clean up legacy files
rm -f /etc/yunying/token
rm -rf /opt/yunying

mkdir -p /etc/yunying /opt/agent-ops/recordings

# Download binary (write to temp then move to avoid partial writes)
echo ">>> Downloading rmux-bridge..."
eval curl -fsSLk ${AUTH} "${BASE_URL}/releases/rmux-bridge-linux-${ARCH}" -o /tmp/rmux-bridge.download
chmod +x /tmp/rmux-bridge.download
mv -f /tmp/rmux-bridge.download /usr/local/bin/rmux-bridge

# Download CA cert
echo ">>> Downloading CA certificate..."
eval curl -fsSLk ${AUTH} "${BASE_URL}/ca.crt" -o /etc/yunying/ca.crt

# Write bridge.env
cat > /etc/yunying/bridge.env << EOF
BRIDGE_AUTH_TOKEN=${BRIDGE_TOKEN}
YUNYING_SERVER_ADDR=${SERVER_ADDR}
YUNYING_CA_CERT=/etc/yunying/ca.crt
RECORDING_ENABLED=true
RECORDING_DIR=/opt/agent-ops/recordings
BRIDGE_AUDIT_DB=/opt/agent-ops/bridge_events.db
RMUX_SOCKET=/root/.rmux/rmux-0/default
EOF

# Install rmux daemon if not present
if ! command -v rmux >/dev/null 2>&1; then
    echo ">>> Installing rmux daemon..."
    curl -fsSL https://rmux.io/install.sh | sh
fi

# Write systemd unit
cat > /etc/systemd/system/rmux-bridge.service << EOF
[Unit]
Description=yunying Bridge
After=network.target rmux-daemon.service

[Service]
EnvironmentFile=/etc/yunying/bridge.env
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
