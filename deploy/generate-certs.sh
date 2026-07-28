#!/bin/bash
# 证书管理：生成 CA 根证书 + 按主机签发独立证书
# 用法:
#   ./generate-certs.sh certs                    # 生成 CA 根证书
#   ./generate-certs.sh certs 10.220.71.1        # 为主机签发证书
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
        -subj "/CN=yunying-ca" \
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

# 生成主机私钥
openssl genrsa -out "$HOST_KEY" 4096
chmod 600 "$HOST_KEY"

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
subjectAltName = DNS:$HOST,IP:$HOST
EOF

openssl req -new -key "$HOST_KEY" -out "$HOST_CSR" -config "$CONF"

# 用 CA 签发
openssl x509 -req -in "$HOST_CSR" \
    -CA "$CA_CRT" -CAkey "$CA_KEY" \
    -CAcreateserial \
    -out "$HOST_CRT" -days 365 \
    -extfile "$CONF" -extensions v3_req

rm -f "$HOST_CSR" "$CONF"
chmod 644 "$HOST_CRT"

# 验证证书链
echo ""
echo "=== Verifying certificate chain ==="
openssl verify -CAfile "$CA_CRT" "$HOST_CRT"

echo ""
echo "Done. Files:"
echo "  MCP client CA:        $CA_CRT   (use with --ca-cert)"
echo "  Bridge cert:           $HOST_CRT"
echo "  Bridge key:            $HOST_KEY"
echo ""
echo "Deploy with:"
echo "  just deploy host=root@$HOST token=<your-token>"
