#!/bin/bash
# 证书管理：生成 CA 根证书 + 按主机签发独立证书
# 用法:
#   ./generate-certs.sh certs                    # 生成 CA 根证书
#   ./generate-certs.sh certs <server-ip>        # 为主机签发证书
#   ./generate-certs.sh certs tf01.example.com   # 支持域名
set -euo pipefail

CERT_DIR="${1:?Usage: $0 <cert-dir> [<host>]}"
HOST="${2:-}"

mkdir -p "$CERT_DIR"

CA_KEY="$CERT_DIR/ca.key"
CA_CRT="$CERT_DIR/ca.crt"

# ─── 生成 CA 根证书 ────────────────────────────
if [ ! -f "$CA_KEY" ] || [ ! -f "$CA_CRT" ]; then
    echo "=== Generating CA root certificate ==="
    openssl req -x509 -newkey rsa:4096 \
        -keyout "$CA_KEY" \
        -out "$CA_CRT" \
        -days 3650 -nodes \
        -subj "/CN=clum-ca" \
        -addext "basicConstraints=critical,CA:TRUE" \
        -addext "keyUsage=critical,keyCertSign,cRLSign"
    chmod 600 "$CA_KEY"
    chmod 644 "$CA_CRT"
    echo "  CA cert: $CA_CRT"
    echo "  CA key:  $CA_KEY (keep secret!)"
fi

if [ -z "$HOST" ]; then
    echo ""
    echo "CA is ready. To issue a certificate for a host:"
    echo "  $0 $CERT_DIR <hostname-or-ip>"
    exit 0
fi

# ─── 为主机签发证书 ───────────────────────────
HOST_KEY="$CERT_DIR/${HOST}.key"
HOST_CRT="$CERT_DIR/${HOST}.crt"
HOST_CSR="$CERT_DIR/${HOST}.csr"

echo "=== Issuing certificate for $HOST ==="

# `host:port` 会命中下面的 `*:*` 分支并被当作 IPv6，直到 x509 签发阶段才报晦涩
# 错误。这里提前拦截，给出可操作的提示。
if [[ "$HOST" =~ ^[^:]+:[0-9]+$ ]]; then
    echo "ERROR: '$HOST' looks like host:port; pass the bare IP or hostname" >&2
    exit 1
fi

# 主机标识可能是 IP 或域名：SAN 类型必须匹配，否则 openssl 会直接失败
# （`IP:example.com` 不是合法 IP）或产出无法被验证的证书。
if [[ "$HOST" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || [[ "$HOST" == *:* ]]; then
    SAN="IP:$HOST"
else
    SAN="DNS:$HOST"
fi

# 主机证书重签会轮换凭据；默认拒绝覆盖，需显式 FORCE=1。
if [ -f "$HOST_KEY" ] || [ -f "$HOST_CRT" ]; then
    if [ "${FORCE:-0}" != "1" ]; then
        echo "ERROR: $HOST_KEY or $HOST_CRT already exists; re-run with FORCE=1 to rotate" >&2
        exit 1
    fi
    echo ">>> FORCE=1: rotating existing host certificate"
fi

# 生成到临时文件：任何一步失败都不会破坏既有的 key/crt 对（FORCE=1 轮换时
# 尤其重要——原地截断旧 key 会让回滚不可能）。
TMP_KEY="${HOST_KEY}.tmp.$$"
TMP_CRT="${HOST_CRT}.tmp.$$"
trap 'rm -f "$TMP_KEY" "$TMP_CRT"' EXIT

openssl genrsa -out "$TMP_KEY" 4096
chmod 600 "$TMP_KEY"

# 生成 CSR + extfile
CONF=$(mktemp)
cat > "$CONF" <<EOF
[req]
distinguished_name = req_distinguished_name
req_extensions = v3_req
prompt = no

[req_distinguished_name]
CN = $HOST

[v3_req]
subjectAltName = $SAN
EOF

openssl req -new -key "$TMP_KEY" -out "$HOST_CSR" -config "$CONF"

# 用 CA 签发
openssl x509 -req -in "$HOST_CSR" \
    -CA "$CA_CRT" -CAkey "$CA_KEY" \
    -CAcreateserial \
    -out "$TMP_CRT" -days 365 \
    -extfile "$CONF" -extensions v3_req

rm -f "$HOST_CSR" "$CONF"
chmod 644 "$TMP_CRT"

# 验证证书链（成功后才替换正式文件）
echo ""
echo "=== Verifying certificate chain ==="
openssl verify -CAfile "$CA_CRT" "$TMP_CRT"

mv -f "$TMP_KEY" "$HOST_KEY"
mv -f "$TMP_CRT" "$HOST_CRT"
trap - EXIT

echo ""
echo "Done. Files:"
echo "  MCP client CA:        $CA_CRT   (use with --ca-cert)"
echo "  Bridge cert:           $HOST_CRT"
echo "  Bridge key:            $HOST_KEY"
echo ""
echo "Deploy with:"
echo "  deploy/deploy-bridge.sh <host> <mode> ...   # see clum-docs/DEPLOY.md"
