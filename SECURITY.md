# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in clum, please **do not** open a public issue.

Instead, email the maintainer directly. We will respond within 48 hours and work on a fix.

## Supported Versions

| Version | Supported          |
|---------|--------------------|
| 0.9.x   | ✅ Supported       |
| < 0.8   | ❌ Not supported   |

## Security Model

clum consists of four components connected over TLS:

1. **clum-mcp** — Central MCP Server: HTTP for AI clients + QUIC for Bridge registration and CLI data plane
2. **clum-cli** — CLI tool for humans to attach to remote sessions via Server relay
3. **rmux-bridge** — Bridge daemon deployed on target Linux hosts, reverse-connects to Central Server
4. **AI clients** — Connect to Central Server via Streamable HTTP with API Key auth

Security assumptions:
- AI clients authenticate via API Key (`yk_{name}_{32hex}`, SHA-256 hashed in SQLite)
- Bridge authentication uses static tokens with constant-time comparison
- All transport is TLS 1.3 encrypted (QUIC built-in + HTTPS)
- CA certificate is mandatory — connections without CA verification are rejected

For production deployments:
- Use a self-managed CA to sign bridge certificates
- Rotate authentication tokens regularly
- Limit bridge access via firewall to trusted IPs only
- Run bridge as a dedicated non-root user when possible

## Security Features

### File Path Protection

Both upload and download operations enforce path safety checks:

- **Bridge-side**: Paths containing `..` are rejected to prevent path traversal attacks. Null bytes are also rejected.
- **MCP-side**: Relative paths returned from the bridge during directory downloads are validated to ensure they don't contain `..` or start with `/`.

### Tunnel Target Whitelist (SSRF Protection)

Hosts can optionally configure `allowed_tunnel_targets` in `hosts.yaml` to restrict which remote host:port combinations are allowed for port forwarding tunnels. If not configured, all targets are allowed (backward compatible).

```yaml
hosts:
  - name: prod-db-01
    bridge_addr: 10.0.1.20:9778
    bridge_token: "your-token"
    allowed_tunnel_targets:
      - "127.0.0.1:5432"    # exact match
      - "10.0.1.*:*"         # glob pattern
      - "*:3306"             # all hosts, MySQL only
```

### Exec Safety Check

The `exec` tool checks terminal state before executing commands. If the terminal is not in `ready` state (e.g., inside vim, less, password prompt), execution is refused to prevent command injection into non-shell contexts.
