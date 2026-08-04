#!/bin/bash
# Deploy or update clum MCP server.
# Usage: deploy/deploy-mcp.sh <binary> <user@host>
#
# Expects /etc/clum/server-config.yaml to already exist on first deploy,
# or writes a minimal one if missing. Certs should be at /etc/clum/.
#
# Includes an idempotent migration prelude: legacy /etc/yunying and
# /root/.yunying layouts are moved to /etc/clum and /root/.clum
# (with a tar backup taken first) before the new binary is installed.
set -euo pipefail

BINARY="${1:?Usage: $0 <binary> <user@host>}"
REMOTE="${2:?Usage: $0 <binary> <user@host>}"
CERTS_DIR="${CERTS_DIR:-certs}"

echo "=== Deploying clum-mcp to $REMOTE ==="

# 0. Migrate legacy yunying layout (idempotent, backup first).
#    Service is stopped before the tar so audit.db is quiescent.
ssh "$REMOTE" 'sudo bash -s' <<'MIGRATE_EOF'
set -e
if { [ -d /etc/yunying ] && [ ! -d /etc/clum ]; } || { [ -d /root/.yunying ] && [ ! -d /root/.clum ]; }; then
    echo ">>> Migrating legacy yunying layout to clum"
    systemctl stop yunying-mcp 2>/dev/null || true
    STAMP=$(date +%Y%m%d-%H%M%S)
    tar czf "/root/clum-migration-backup-${STAMP}.tar.gz" \
        $( [ -d /etc/yunying ] && echo /etc/yunying ) \
        $( [ -d /root/.yunying ] && echo /root/.yunying ) 2>/dev/null || true
    echo ">>> Backup: /root/clum-migration-backup-${STAMP}.tar.gz"
    if [ -d /etc/yunying ] && [ ! -d /etc/clum ]; then
        mv /etc/yunying /etc/clum
        sed -i 's|/etc/yunying|/etc/clum|g; s|/root/.yunying|/root/.clum|g' /etc/clum/server-config.yaml 2>/dev/null || true
    fi
    if [ -d /root/.yunying ] && [ ! -d /root/.clum ]; then
        mv /root/.yunying /root/.clum
    fi
fi
MIGRATE_EOF

# 1. Create directories
ssh "$REMOTE" "sudo mkdir -p /etc/clum /root/.clum/recordings"

# 2. Upload binary
scp "$BINARY" "$REMOTE:/tmp/clum-mcp.new"
ssh "$REMOTE" "sudo mv /tmp/clum-mcp.new /usr/local/bin/clum-mcp && sudo chmod 755 /usr/local/bin/clum-mcp"

# 3. Upload certs if they exist locally and not on remote
for f in ca.crt server.crt server.key; do
    if [ -f "$CERTS_DIR/$f" ]; then
        scp "$CERTS_DIR/$f" "$REMOTE:/tmp/$f"
        ssh "$REMOTE" "sudo mv /tmp/$f /etc/clum/$f"
    fi
done
ssh "$REMOTE" "sudo chmod 600 /etc/clum/server.key 2>/dev/null; sudo chmod 644 /etc/clum/ca.crt /etc/clum/server.crt 2>/dev/null" || true

# 4. Ensure hosts.yaml exists (never overwrite — the server-side file is
# authoritative; bridges enroll into the runtime registry DB).
ssh "$REMOTE" "test -f /etc/clum/hosts.yaml || (echo 'hosts: []' | sudo tee /etc/clum/hosts.yaml > /dev/null && sudo chmod 600 /etc/clum/hosts.yaml)"

# 5. Write server-config.yaml if not present
ssh "$REMOTE" "test -f /etc/clum/server-config.yaml" 2>/dev/null || {
    echo "Writing default server-config.yaml..."
    ssh "$REMOTE" "sudo tee /etc/clum/server-config.yaml" <<'CONFIG_EOF'
listen: "0.0.0.0:9788"
server_cert: "/etc/clum/server.crt"
server_key: "/etc/clum/server.key"
ca_cert: "/etc/clum/ca.crt"
hosts_file: "/etc/clum/hosts.yaml"
audit_db: "/root/.clum/audit.db"
static_dir: "/root/.clum"
recordings_dir: "/root/.clum/recordings"

audit_retention_days: 90
audit_max_size_mb: 500
audit_cleanup_interval_secs: 600
audit_sync_interval_secs: 300

recordings_retention_days: 90
recordings_max_size_mb: 5000
CONFIG_EOF
}

# 6. Write systemd unit (tmp + atomic mv, then reload)
ssh "$REMOTE" "sudo tee /tmp/clum-mcp.service" <<'UNIT_EOF'
[Unit]
Description=clum MCP Server
After=network.target

[Service]
ExecStart=/usr/local/bin/clum-mcp --mode http --config /etc/clum/server-config.yaml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT_EOF
ssh "$REMOTE" "sudo mv /tmp/clum-mcp.service /etc/systemd/system/clum-mcp.service"

# 7. Kill any existing nohup process (legacy or new name)
ssh "$REMOTE" "pkill -f 'clum-mcp.*--mode' 2>/dev/null; pkill -f 'yunying-mcp.*--mode' 2>/dev/null; sleep 1" || true

# 8. Start via systemd; disable legacy unit (file kept for rollback)
ssh "$REMOTE" "sudo systemctl daemon-reload && sudo systemctl enable --now clum-mcp && (sudo systemctl disable yunying-mcp 2>/dev/null || true)"

echo ""
echo "=== MCP Server deployment complete ==="
echo "Check: ssh $REMOTE systemctl status clum-mcp"
