# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in clum, please **do not** open a public issue.

Instead, email the maintainer directly. We will respond within 48 hours and work on a fix.

## Supported Versions

| Version | Supported          |
|---------|--------------------|
| 0.17.x  | ✅ Supported       |
| 0.16.x  | ✅ Supported       |
| 0.15.x  | ✅ Supported       |
| 0.14.x  | ✅ Supported       |
| 0.13.x  | ⚠️ Legacy         |
| 0.11.x-0.12.x | ⚠️ Legacy       |
| 0.10.x  | ⚠️ Legacy (rename transition, compat shims only) |
| < 0.10  | ❌ Not supported   |

## Security Model

clum consists of four components connected over TLS:

1. **clum-mcp** — Central MCP Server: HTTP for AI clients + QUIC for Bridge registration and CLI data plane
2. **clum-cli** — CLI tool for humans to attach to remote sessions via Server relay
3. **rmux-bridge** — Bridge daemon deployed on target Linux hosts, reverse-connects to Central Server
4. **AI clients** — Connect to Central Server via Streamable HTTP with API Key auth

Security assumptions:
- AI clients authenticate via API Key (`yk_{name}_{64hex}`, SHA-256 hashed in SQLite)
- Bridge authentication uses static tokens: direct-mode connections compare tokens in constant time; enrolled bridges authenticate at registration via a SHA-256 token-hash lookup against the server's token map (revocation evicts the map entry)
- QUIC transport (Server↔Bridge, Server↔CLI) is TLS 1.3 encrypted (mandatory in the QUIC protocol)
- The MCP HTTPS endpoint uses rustls (TLS 1.2+, negotiates 1.3 by default); HTTP mode is TLS-only (fail-closed) — startup fails if `--server-cert`/`--server-key` are missing, never falls back to plain HTTP
- CA verification is enforced on all connections: server→bridge direct-mode connections require `--ca-cert` and fail closed without it; in pure enrolled deployments (bridges initiate the connection) the server's `--ca-cert` can be omitted. The former `--insecure` flag has been removed; skipping TLS verification is not supported

For production deployments:
- Use a self-managed CA to sign bridge certificates
- Rotate authentication tokens regularly
- Limit bridge access via firewall to trusted IPs only
- Run bridge as a dedicated non-root user when possible

## Security Features

### File Path Protection

Both upload and download operations enforce path safety checks:

- **Bridge-side**: Paths containing `..` are rejected to prevent path traversal attacks. Null bytes are also rejected.
- **MCP-side**: Relative paths returned from the bridge during directory downloads are validated to ensure they don't contain `..` or start with `/`. The `local_path`/`local_dir` uploads and download destinations are also validated server-side (rejects `..` and null bytes).

### Tunnel Target Whitelist (SSRF Protection)

Hosts can optionally configure `allowed_forward_targets` in `hosts.yaml` to restrict which remote host:port combinations are allowed for port forwarding forwards. If not configured, all targets are allowed (backward compatible). Note: the whitelist applies only to hosts defined in `hosts.yaml` — a dynamically-enrolled bridge with no matching `hosts.yaml` entry has no configurable whitelist and allows all targets.

```yaml
hosts:
  - name: prod-db-01
    bridge_addr: 10.0.1.20:9778
    bridge_token: "your-token"
    allowed_forward_targets:
      - "127.0.0.1:5432"    # exact match
      - "10.0.1.*:*"         # glob pattern
      - "*:3306"             # all hosts, MySQL only
```

### Exec Safety Check

The `exec` tool checks terminal state before executing commands. If the terminal is not in `ready` state (e.g., inside vim, less, password prompt), execution is refused to prevent command injection into non-shell contexts.

**Command Injection Prevention (v0.17.1+)**:

Starting from version 0.17.1, `exec` enforces strict validation of command parameters to prevent command injection attacks:

- **Control Character Rejection**: Commands containing dangerous control characters (`\n`, `\r`, `\x00`-`\x1f`, `\x7f`) are rejected with clear error messages directing users to use `shell_command` for complex scenarios
- **Shell Metacharacter Warning**: Commands with pipes (`|`), redirects (`>`, `<`), or operators (`&&`, `||`, `;`) trigger warnings but are allowed (for backward compatibility with existing workflows)
- **Tool Separation**: Use `exec` for simple commands and `shell_command` for complex shell scripts requiring pipes, redirects, or multi-line logic

Example rejection scenarios:
```bash
# Blocked - newline injection
exec: "ls\nrm -rf /"
Error: "exec rejected: command contains newline/carriage return (0x0a). Use shell_command tool for multi-line scripts or complex commands."

# Blocked - control characters
exec: "ls\x00foo"
Error: "exec rejected: command contains unsafe control character 0x00. Control characters are not allowed."
```

The `shell_command` tool internally uses rmux SDK's shell handling, which safely executes complex commands through proper shell escaping.

### Sensitive Input Redaction

Input tools (`send_keys`, `send_text`, `broadcast_keys`, `batch_send_keys`) accept a `sensitive` flag that redacts the audit `detail` to `[REDACTED:N bytes]`. When the terminal is in `password` state (detected via a pre-injection snapshot), redaction is enforced server-side regardless of the flag — there is no opt-out. Audit events carry a `redacted` marker; plaintext credentials are never written to the audit database.

This applies to the **audit trail only**. PTY recordings (below) are a separate surface: their *content* remains unmasked — passwords appear as cleartext input events — but recording files are now encrypted at rest (see below).

### PTY Session Recordings (Encrypted at rest)

Bridge-side PTY recordings (asciinema v2 content) faithfully capture everything typed into and displayed by the terminal — **including passwords in cleartext within the recording content**. Since 2026-09-11, recording files are encrypted at rest: each file is an envelope with a plaintext `clum-enc` header line plus per-recording data sealed with X25519 ECDH (server-held keypair, public key delivered to the bridge at registration) and chunked AES-256-GCM. Both the bridge-side files and the server-side synced copies are stored as ciphertext.

Known boundary: in direct mode, or if the server's recording public key is unavailable at bridge startup, the recorder falls back to plaintext recording (a fail-open path with a logged warning). Content masking is intentionally not performed — at-rest encryption protects the file, not the fields inside it.

Operational guidance: manage production hosts via `NOPASSWD` sudoers or SSH keys so that credentials never enter a terminal session — anything typed there appears as cleartext inside the (encrypted) recording, in every synced copy, for as long as the file is retained.

### HTTP Endpoint Protection

- **Static file serving**: The `/releases/` download endpoint rejects path components containing `..`, preventing traversal outside the release directory.
- **Download tokens**: Bridge binaries and certificates are gated behind download tokens with a configurable TTL (`token_ttl_hours` in server-config.yaml, default 24 hours), in addition to API Key auth on the `/mcp` endpoint.
