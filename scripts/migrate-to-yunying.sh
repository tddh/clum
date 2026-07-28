#!/bin/bash
# 将 ~/.agent-ops/ 迁移到 ~/.yunying/
# 用法: bash scripts/migrate-to-yunying.sh
set -e

OLD_DIR="$HOME/.agent-ops"
NEW_DIR="$HOME/.yunying"

if [ ! -d "$OLD_DIR" ]; then
  echo "No $OLD_DIR found, nothing to migrate."
  exit 0
fi

if [ -d "$NEW_DIR" ]; then
  echo "$NEW_DIR already exists. Merge contents from $OLD_DIR? (y/n)"
  read -r ans
  [ "$ans" = "y" ] || exit 1
  cp -rn "$OLD_DIR"/* "$NEW_DIR"/ 2>/dev/null || true
  echo "Merged. Old directory preserved at $OLD_DIR (remove manually when ready)."
else
  mv "$OLD_DIR" "$NEW_DIR"
  echo "Migrated: $OLD_DIR → $NEW_DIR"
fi

echo "Contents:"
ls -la "$NEW_DIR"
