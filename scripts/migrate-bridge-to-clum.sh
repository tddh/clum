#!/bin/bash
# One-shot bridge migration to clum (0.10.0): binary upgrade + path migration.
# Preserves the existing BRIDGE_AUTH_TOKEN in bridge.env (no token re-issue needed).
#
# Usage: scripts/migrate-bridge-to-clum.sh <bridge-binary> <user@host>
set -euo pipefail

BINARY="${1:?Usage: $0 <bridge-binary> <user@host>}"
REMOTE="${2:?Usage: $0 <bridge-binary> <user@host>}"

echo "=== Migrating bridge to clum on $REMOTE ==="

scp "$BINARY" "$REMOTE:/tmp/rmux-bridge.new"

ssh "$REMOTE" 'sudo bash -s' <<'REMOTE_EOF'
set -e
systemctl stop rmux-bridge 2>/dev/null || true

STAMP=$(date +%Y%m%d-%H%M%S)
tar czf "/root/clum-migration-backup-${STAMP}.tar.gz" \
    $( [ -d /etc/yunying ] && echo /etc/yunying ) \
    $( [ -d /opt/agent-ops ] && echo /opt/agent-ops ) \
    /etc/systemd/system/rmux-bridge.service /usr/local/bin/rmux-bridge 2>/dev/null || true
echo ">>> Backup: /root/clum-migration-backup-${STAMP}.tar.gz"

# Path migration (skip if /etc/yunying is just a compat symlink)
if [ -L /etc/yunying ]; then
    rm /etc/yunying
elif [ -d /etc/yunying ] && [ ! -d /etc/clum ]; then
    mv /etc/yunying /etc/clum
fi
mkdir -p /etc/clum
if [ -d /opt/agent-ops ] && [ ! -d /opt/clum ]; then
    mv /opt/agent-ops /opt/clum
fi
if [ -f /etc/profile.d/yunying.sh ] && [ ! -f /etc/profile.d/clum.sh ]; then
    mv /etc/profile.d/yunying.sh /etc/profile.d/clum.sh
fi

# Rewrite bridge.env: env key names + paths
if [ -f /etc/clum/bridge.env ]; then
    sed -i 's/^YUNYING_/CLUM_/g; s|/etc/yunying|/etc/clum|g; s|/opt/agent-ops|/opt/clum|g' /etc/clum/bridge.env
fi

# Install new binary
mv /tmp/rmux-bridge.new /usr/local/bin/rmux-bridge
chmod 755 /usr/local/bin/rmux-bridge

# Rewrite systemd unit
cat > /tmp/rmux-bridge.service <<'UNIT_EOF'
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
UNIT_EOF
mv /tmp/rmux-bridge.service /etc/systemd/system/rmux-bridge.service

systemctl daemon-reload
systemctl restart rmux-bridge
REMOTE_EOF

sleep 3
echo "Service state: $(ssh "$REMOTE" 'systemctl is-active rmux-bridge')"
echo "=== Migration sent. Verify: host_list shows online + journalctl -u rmux-bridge ==="
