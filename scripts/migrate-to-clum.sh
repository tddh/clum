#!/bin/bash
# 将 ~/.yunying/ 迁移到 ~/.clum/（本机客户端数据目录）。
# 用法: bash scripts/migrate-to-clum.sh
#
# 幂等：旧目录不存在则直接退出；新目录已存在则提示手动合并。
# 旧目录迁移后保留（重命名为 ~/.yunying.bak-<date>），确认无误后可自行删除。
set -e

OLD_DIR="$HOME/.yunying"
NEW_DIR="$HOME/.clum"

if [ ! -d "$OLD_DIR" ]; then
    echo "No $OLD_DIR found, nothing to migrate."
else
    if [ -d "$NEW_DIR" ]; then
        echo "$NEW_DIR already exists. Please merge manually:"
        echo "  ls $OLD_DIR $NEW_DIR"
        exit 1
    fi
    mv "$OLD_DIR" "$NEW_DIR"
    echo "Migrated: $OLD_DIR → $NEW_DIR"
fi

echo ""
echo "Contents of $NEW_DIR:"
ls -la "$NEW_DIR" 2>/dev/null || true

echo ""
echo ">>> 检查 shell 配置中的旧环境变量（YUNYING_*），请手动改为 CLUM_*："
for f in "$HOME/.envrc.local" "$HOME/.envrc" "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile"; do
    if [ -f "$f" ] && grep -q "YUNYING_" "$f"; then
        echo "  $f:"
        grep -n "YUNYING_" "$f" | sed 's/^/    /'
    fi
done
echo ""
echo "Done. 新的环境变量：CLUM_SERVER_ADDR / CLUM_API_KEY（旧 YUNYING_* 仍作为 fallback 生效）。"
