#!/bin/sh
set -e

# yunying Bridge installer
# Usage: curl -fsSL https://SERVER:9778/install.sh | BRIDGE_TOKEN=xxx SERVER_ADDR=10.0.0.1:9778 sh

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

SERVER_HOST=$(echo "$SERVER_ADDR" | cut -d: -f1)
SERVER_PORT=$(echo "$SERVER_ADDR" | cut -d: -f2)
SERVER_PORT=${SERVER_PORT:-9778}
BASE_URL="http://${SERVER_HOST}:${SERVER_PORT}"

echo ">>> Installing yunying bridge (${ARCH})"
echo ">>> Server: ${SERVER_ADDR}"

mkdir -p /etc/yunying

# Download binary
echo ">>> Downloading rmux-bridge..."
curl -fsSL "${BASE_URL}/releases/rmux-bridge-linux-${ARCH}" -o /usr/local/bin/rmux-bridge
chmod +x /usr/local/bin/rmux-bridge

# Download CA cert (default: yes, since we use private CA)
if [ "${SKIP_CA}" != "1" ]; then
    echo ">>> Downloading CA certificate..."
    curl -fsSL "${BASE_URL}/ca.crt" -o /etc/yunying/ca.crt
    CA_FLAG="--ca-cert /etc/yunying/ca.crt"
else
    CA_FLAG=""
fi

# Write token
echo "${BRIDGE_TOKEN}" > /etc/yunying/token
chmod 600 /etc/yunying/token

# Install rmux daemon if not present
if ! command -v rmux >/dev/null 2>&1; then
    echo ">>> Installing rmux daemon..."
    curl -fsSL https://rmux.io/install.sh | sh
fi

# Write systemd unit
cat > /etc/systemd/system/rmux-bridge.service << EOF
[Unit]
Description=yunying Bridge
After=network.target

[Service]
Environment=BRIDGE_AUTH_TOKEN=${BRIDGE_TOKEN}
Environment=YUNYING_SERVER_ADDR=${SERVER_ADDR}
ExecStart=/usr/local/bin/rmux-bridge --server-addr ${SERVER_ADDR} --auth-token ${BRIDGE_TOKEN} --rmux-socket /root/.rmux/rmux-0/default ${CA_FLAG}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now rmux-bridge

echo ">>> Bridge installed and started."
echo ">>> Check: systemctl status rmux-bridge"
