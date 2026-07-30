#!/bin/bash
# Update bridge binary only (no config changes).
# Usage: deploy/update-bridge.sh <binary> <user@host>
set -euo pipefail

BINARY="${1:?Usage: $0 <binary> <user@host>}"
REMOTE="${2:?Usage: $0 <binary> <user@host>}"

echo "=== Updating rmux-bridge on $REMOTE ==="

# Atomic replace: upload to .new, then mv
scp "$BINARY" "$REMOTE:/tmp/rmux-bridge.new"
ssh "$REMOTE" "sudo mv /tmp/rmux-bridge.new /usr/local/bin/rmux-bridge && sudo chmod 755 /usr/local/bin/rmux-bridge"

# Restart
ssh "$REMOTE" "sudo systemctl restart rmux-bridge"

echo "=== Update complete ==="
ssh "$REMOTE" "systemctl is-active rmux-bridge"
