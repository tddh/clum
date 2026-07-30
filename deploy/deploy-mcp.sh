#!/bin/bash
# Deploy or update yunying MCP server.
# Usage: deploy/deploy-mcp.sh <binary> <user@host>
#
# Expects /etc/yunying/server-config.yaml to already exist on first deploy,
# or writes a minimal one if missing. Certs should be at /etc/yunying/.
set -euo pipefail

BINARY="${1:?Usage: $0 <binary> <user@host>}"
REMOTE="${2:?Usage: $0 <binary> <user@host>}"
CERTS_DIR="${CERTS_DIR:-certs}"

echo "=== Deploying yunying-mcp to $REMOTE ==="

# 1. Create directories
ssh "$REMOTE" "sudo mkdir -p /etc/yunying /root/.yunying/recordings"

# 2. Upload binary
scp "$BINARY" "$REMOTE:/tmp/yunying-mcp.new"
ssh "$REMOTE" "sudo mv /tmp/yunying-mcp.new /usr/local/bin/yunying-mcp && sudo chmod 755 /usr/local/bin/yunying-mcp"

# 3. Upload certs if they exist locally and not on remote
for f in ca.crt server.crt server.key; do
    if [ -f "$CERTS_DIR/$f" ]; then
        scp "$CERTS_DIR/$f" "$REMOTE:/tmp/$f"
        ssh "$REMOTE" "sudo mv /tmp/$f /etc/yunying/$f"
    fi
done
ssh "$REMOTE" "sudo chmod 600 /etc/yunying/server.key 2>/dev/null; sudo chmod 644 /etc/yunying/ca.crt /etc/yunying/server.crt 2>/dev/null" || true

# 4. Upload hosts.yaml
HOSTS_FILE="${HOSTS_FILE:-config/hosts.yaml}"
if [ -f "$HOSTS_FILE" ]; then
    scp "$HOSTS_FILE" "$REMOTE:/tmp/hosts.yaml"
    ssh "$REMOTE" "sudo mv /tmp/hosts.yaml /etc/yunying/hosts.yaml && sudo chmod 600 /etc/yunying/hosts.yaml"
fi

# 5. Write server-config.yaml if not present
ssh "$REMOTE" "test -f /etc/yunying/server-config.yaml" 2>/dev/null || {
    echo "Writing default server-config.yaml..."
    ssh "$REMOTE" "sudo tee /etc/yunying/server-config.yaml" <<'CONFIG_EOF'
listen: "0.0.0.0:9788"
server_cert: "/etc/yunying/server.crt"
server_key: "/etc/yunying/server.key"
ca_cert: "/etc/yunying/ca.crt"
hosts_file: "/etc/yunying/hosts.yaml"
audit_db: "/root/.yunying/audit.db"
static_dir: "/root/.yunying"
recordings_dir: "/root/.yunying/recordings"

audit_retention_days: 90
audit_max_size_mb: 500
audit_cleanup_interval_secs: 600
audit_sync_interval_secs: 300

recordings_retention_days: 90
recordings_max_size_mb: 5000
CONFIG_EOF
}

# 6. Write systemd unit
ssh "$REMOTE" "sudo tee /etc/systemd/system/yunying-mcp.service" <<'UNIT_EOF'
[Unit]
Description=yunying MCP Server
After=network.target

[Service]
ExecStart=/usr/local/bin/yunying-mcp --mode http --config /etc/yunying/server-config.yaml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT_EOF

# 7. Kill any existing nohup process
ssh "$REMOTE" "pkill -f 'yunying-mcp.*--mode' 2>/dev/null; sleep 1" || true

# 8. Start via systemd
ssh "$REMOTE" "sudo systemctl daemon-reload && sudo systemctl enable --now yunying-mcp"

echo ""
echo "=== MCP Server deployment complete ==="
echo "Check: ssh $REMOTE systemctl status yunying-mcp"
