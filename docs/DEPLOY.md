# agent-ops 部署文档

> 最后更新：2026-07-14

## 架构

```
                               QUIC :9778  终端操作 + 文件传输
┌─────────────────┐  MCP stdio  ┌──────────────┐ ════════════════════════╗ ┌──────────────────┐   Unix Socket  ┌─────────┐
│  AI 客户端        │◄─────────►│ agent-ops-mcp │                       ║ │   rmux-bridge     │◄─────────────►│  RMUX   │
│ (OpenCode/Claude) │            │  (macOS/Linux/Windows) │ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ║ │   (Linux 远程主机)  │                │ daemon  │
└─────────────────┘            └──────────────┘ ════════════════════════╝ └──────────────────┘                └─────────┘
```

- **agent-ops-mcp**: MCP Server，运行在 AI 客户端同机，提供 66 个终端控制工具 + 操作审计 CLI
- **agent-ops-cli**: 命令行工具，人可以直接 PTY 透传 attach 到远程 rmux 会话（`agent-ops-cli connect`）
- **rmux-bridge**: 部署在每台目标 Linux 主机上，QUIC 加密代理 → RMUX daemon。终端操作与文件传输统一走 QUIC 协议（UDP :9778）
- **RMUX daemon**: 每个 Linux 主机上的终端多路复用器

## 前置条件

| 组件 | 要求 |
|------|------|
| 目标主机 | Linux x86_64，systemd，有 SSH 访问 |
| RMUX | `rmux` 0.9+ daemon 已安装并运行（`curl -fsSL https://rmux.io/install.sh \| sh`） |
| 构建机 | Rust 1.85+，`x86_64-linux-musl-gcc`（交叉编译用 `brew install FiloSottile/musl-cross/musl-cross`） |
| 端口 | bridge 监听 9778（QUIC/UDP） |
| 证书 | 自签名 TLS 证书（`openssl` 即可） |

## 快速开始

### 1. 构建

```bash
# 本机构建（macOS 开发）
cargo build -p agent-ops-mcp --release
cargo build -p agent-ops-cli --release

# 交叉编译 bridge（Linux x86_64，静态链接）
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc \
  cargo build --target x86_64-unknown-linux-musl --release -p rmux-bridge

# 或用 just 快捷命令
just release-linux          # 交叉编译 bridge + mcp（Linux 目标）
just build-mcp              # 本机构建 mcp
```

构建产物：
- `target/release/agent-ops-mcp` — MCP server（本地运行）
- `target/release/agent-ops-cli` — 命令行工具（本地运行）
- `target/x86_64-unknown-linux-musl/release/rmux-bridge` — bridge（部署到远程）

### 2. 部署

部署分两步：先部署 `rmux-daemon`，再部署 `rmux-bridge`。

**步骤 2a：部署 daemon**

```bash
bash deploy/install-daemon.sh root@<your-bridge-ip>
```

做的事：
- 安装 rmux（如未安装）
- 上传项目定制的 `rmux-daemon.service`（配置 `RMUX_TMPDIR=%h/.rmux`）
- 上传项目定制的 `rmux.conf` 到 `/root/.rmux.conf`（启用鼠标、调大回滚缓冲区、**开启 `allow-passthrough`**——rmux 0.9 默认为 `off`，不开启会导致 CLI 连接后按键无效）
- 启动 daemon
- 写入 `/etc/profile.d/agent-ops.sh`（`export RMUX_TMPDIR=$HOME/.rmux`），用户登录后可直接 `rmux a -t agent-ops`

**步骤 2b：部署 bridge**

```bash
# 一键：生成证书 → 上传二进制 → 配置 systemd → 启动
just deploy host=root@<your-bridge-ip> token=<your-token>

# 或手动：
BRIDGE_TOKEN="<your-token>" bash deploy/install-bridge.sh \
  ./target/x86_64-unknown-linux-musl/release/rmux-bridge \
  root@<your-bridge-ip>
```

部署脚本自动完成：
- 用 `deploy/generate-certs.sh` 在本地生成主机专属 TLS 证书（`certs/<ip>.crt` / `certs/<ip>.key`）
- 上传 `rmux-bridge` 二进制到 `/opt/agent-ops/`
- 上传证书到 `/opt/agent-ops/certs/`
- 写入 token 到 `/opt/agent-ops/bridge.env`（权限 600）
- 创建 `rmux-bridge.service`（`systemctl enable --now`）

**其他 Justfile 命令：**

| 命令 | 说明 |
|------|------|
| `just certs` | 生成本地测试用自签名证书 |
| `just certs-host host=<name>` | 为指定主机生成 TLS 证书 |
| `just run-bridge token=<your-token>` | 本地启动 bridge（开发/测试） |

**生成的 systemd 服务文件**（`/etc/systemd/system/rmux-bridge.service`）：

```ini
[Unit]
Description=RMUX Bridge - QUIC to Unix socket proxy
After=network.target rmux-daemon.service
Requires=rmux-daemon.service

[Service]
Type=simple
EnvironmentFile=/opt/agent-ops/bridge.env
ExecStart=/opt/agent-ops/rmux-bridge \
    --quic-listen-addr 0.0.0.0:9778 \
    --max-connections 256 \
    --rmux-socket /root/.rmux/rmux-0/default \
    --tls-cert /opt/agent-ops/certs/<ip>.crt \
    --tls-key /opt/agent-ops/certs/<ip>.key
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

### 3. 更新 bridge（已有安装）

```bash
# 交叉编译
just release-linux

# 替换二进制 + 重启
ssh root@<your-bridge-ip> "systemctl stop rmux-bridge"
scp target/x86_64-unknown-linux-musl/release/rmux-bridge root@<your-bridge-ip>:/opt/agent-ops/
ssh root@<your-bridge-ip> "systemctl start rmux-bridge"

# 验证
ssh root@<your-bridge-ip> "systemctl status rmux-bridge --no-pager"
```

### 4. Bridge CLI 参数参考

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--quic-listen-addr` | `0.0.0.0:9778` | QUIC/UDP 监听地址（终端操作 + 文件传输） |
| `--max-connections` | `256` | 最大并发连接数，0=无限制（`MAX_CONNECTIONS` 环境变量） |
| `--rmux-socket` | `/tmp/rmux-1000/default` | RMUX daemon Unix socket 路径 |
| `--tls-cert` | `certs/bridge.crt` | TLS 证书路径（CA 签发） |
| `--tls-key` | `certs/bridge.key` | TLS 私钥路径 |
| `--auth-token` | 环境变量 `BRIDGE_AUTH_TOKEN` | 认证令牌 |
| `--log-level` | `info` | 日志级别：trace/debug/info/warn/error（`RUST_LOG` 环境变量） |
| `--idle-timeout-secs` | `28800` | 交互式空闲超时（秒），超时后断连并恢复 pane 布局。0=禁用（`IDLE_TIMEOUT_SECS` 环境变量） |

> **QUIC 协议**：所有通信走 QUIC（UDP :9778），内置 TLS 1.3 加密。确保防火墙放行 UDP 9778 端口。

### 5. MCP Server CLI 参数参考

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--hosts-file` | `config/hosts.yaml` | 主机注册表路径 |
| `--ca-cert` | 无 | CA 证书路径（必填，不传则拒绝连接） |
| `--audit-db` | `~/.agent-ops/audit.db` | 审计数据库路径 |
| `--audit-retention-days` | `90` | 审计数据保留天数 |
| `--audit-max-size-mb` | `500` | 审计数据库大小上限 (MB) |
| `--audit-cleanup-interval-secs` | `600` | 自动清理间隔（秒） |
| `--audit-sync-interval-secs` | `300` | 录制文件同步拉取间隔（秒） |
| `--recordings-dir` | `~/.agent-ops/recordings` | 本地录制存储目录 |
| `--recordings-retention-days` | `90` | 本地录制保留天数 |
| `--recordings-max-size-mb` | `5000` | 本地录制容量上限 (MB) |

### 6. 认证模式

Bridge 使用静态 token 认证，通过常数时间比较（防时序攻击）。

```yaml
# config/hosts.yaml
hosts:
  - name: tf01
    bridge_addr: 10.0.1.10:9778
    bridge_token: "your-secure-token"
```

`bridge_token` 和系统环境变量 `BRIDGE_AUTH_TOKEN` 中的 token 必须一致。

### 7. 配置主机注册表

创建 `config/hosts.yaml`：

```yaml
hosts:
  - name: tf01                         # MCP 工具中引用的主机名
    bridge_addr: <your-bridge-ip>:9778     # bridge 地址
    bridge_token: "<your-token>"              # 认证 token
    group: production                  # 分组（host_filter 用）
    tags: [web, nginx]                  # 标签
    labels:                             # 键值对标签
      dc: shanghai
      rack: a3
    # 可选：限制隧道目标（不配置 = 全部允许）
    # allowed_tunnel_targets:
    #   - "127.0.0.1:5432"             # 精确匹配
    #   - "10.0.1.*:*"                 # glob 通配符
    #   - "*:3306"                     # 所有主机的 MySQL
```

> 💡 **热加载**：修改 `hosts.yaml` 后无需重启 MCP Server — 调用 `reload_config` MCP 工具或向进程发送 `kill -HUP <pid>` 即可生效。加载失败时保留原有配置，不影响运行中服务。

### 8. 配置 MCP Server（OpenCode）

编辑 `~/.config/opencode/opencode.json`：

```json
{
  "mcp": {
    "agent-ops": {
      "type": "local",
      "command": ["/path/to/agent-ops/target/release/agent-ops-mcp"],
      "args": [
        "--hosts-file",
        "/path/to/agent-ops/config/hosts.test.yaml",
        "--ca-cert",
        "/tmp/bridge-remote.crt"
      ],
      "enabled": true
    }
  }
}
```

### 9. 验证

```bash
# 直接调 MCP 测试
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"host_list","arguments":{}}}' \
  | target/release/agent-ops-mcp --hosts-file config/hosts.test.yaml --ca-cert /tmp/bridge-remote.crt 2>/dev/null
```

信任首次连接：将远程 bridge 的 `bridge.crt` 复制到本地，通过 `--ca-cert` 参数指定。

## 运维

```bash
# 查看 bridge 状态
ssh root@<your-bridge-ip> "systemctl status rmux-bridge"

# 查看日志
ssh root@<your-bridge-ip> "journalctl -u rmux-bridge -f"

# 检查 RMUX socket 是否存在
ssh root@<your-bridge-ip> "ls -la \$HOME/.rmux/rmux-*/default"
```

### 审计查询

```bash
# 查最近操作
agent-ops-mcp audit query --format table

# 查特定主机的命令执行记录
agent-ops-mcp audit query --host tf01 --action exec --since 2026-06-01

# 统计概览
agent-ops-mcp audit stats

# 手动清理
agent-ops-mcp audit cleanup --older-than 30
```

审计数据默认存储在 `~/.agent-ops/audit.db`，保留 90 天，上限 500MB。

## 目录结构

```
~/.agent-ops/                      # MCP Server 本地
├── audit.db                       # 审计数据库（SQLite）

/opt/agent-ops/                   # 远程主机
├── rmux-bridge                   # bridge 二进制
├── bridge.env                    # BRIDGE_AUTH_TOKEN（权限 600）
└── certs/
    ├── <ip>.crt                  # 主机 TLS 证书
    └── <ip>.key                  # TLS 私钥（权限 600）

/etc/systemd/system/
├── rmux-daemon.service           # daemon systemd 服务
└── rmux-bridge.service           # bridge systemd 服务

/etc/profile.d/
└── agent-ops.sh                  # RMUX_TMPDIR 环境变量
```

## 故障排查

| 症状 | 检查 |
|------|------|
| CLI `connect` 后按键无效、终端卡死 | rmux 0.9 将 `allow-passthrough` 默认改为 `off`。检查 `/root/.rmux.conf` 中是否有 `set -g allow-passthrough on`，然后 `systemctl restart rmux-daemon`。项目已提供默认配置 `config/rmux.conf`。 |
| MCP 工具返回 `connection refused` | `systemctl status rmux-bridge`，确认 bridge 在运行 |
| `authentication failed` | 检查 `bridge.env` 中的 `BRIDGE_AUTH_TOKEN` 与 `hosts.yaml` 中 `bridge_token` 是否一致 |
| TLS 握手失败 | `--ca-cert` 指向的证书是否与 bridge 端一致 |
| `unknown request type` | bridge 版本过旧，重新交叉编译部署 |
| RMUX socket 找不到 | `ls $HOME/.rmux/rmux-*/default`，确认 rmux daemon 在运行（socket 路径由 `RMUX_TMPDIR` 环境变量控制，项目 daemon service 配置为 `$HOME/.rmux`，部署脚本自动检测实际路径） |

## 安全

### Unix Socket

rmux daemon 的 socket 路径由 `RMUX_TMPDIR` 环境变量控制。项目定制的 `rmux-daemon.service` 设置 `RMUX_TMPDIR=%h/.rmux`（root 用户展开为 `/root/.rmux`），socket 位于 `$RMUX_TMPDIR/rmux-<UID>/default`。

部署脚本自动检测实际 socket 路径，无需手动指定。Socket 权限为 `srw-------`（仅 owner 可读写），其他用户无法访问。

如果需要在自定义路径运行 rmux daemon，同步更新：
- `rmux-daemon.service` 中的 `Environment=RMUX_TMPDIR=...`
- `/etc/profile.d/agent-ops.sh` 中的 `export RMUX_TMPDIR=...`
- bridge 的 `--rmux-socket` 参数（部署脚本自动检测）

### TLS 安全模式

只有一种模式：

| 模式 | 触发条件 | 安全等级 |
|------|---------|:---:|
| CA 验证 | `--ca-cert /path/to/ca.crt` | ✅ 验证服务器身份，防中间人 |
| 拒绝连接 | 未提供 CA | 🔒 默认行为 |

> `--insecure` 参数已移除（commit 4dc02183），不再支持跳过 TLS 证书验证。

### 自签名证书

**生产环境建议**：自建 CA，为每台 bridge 签发证书，MCP server 只持有 CA 根证书。

```bash
# 生成 CA
openssl req -x509 -newkey rsa:4096 -keyout ca.key -out ca.crt -days 3650 -nodes \
  -subj "/CN=agent-ops-ca" -addext "basicConstraints=critical,CA:TRUE"

# 为 bridge 签发（替换 <your-bridge-ip> 为实际 IP）
openssl req -new -newkey rsa:2048 -keyout bridge.key -out bridge.csr -nodes \
  -subj "/CN=<your-bridge-ip>" -addext "subjectAltName=DNS:<your-bridge-ip>,IP:<your-bridge-ip>"
openssl x509 -req -in bridge.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out bridge.crt -days 365

# MCP server 启动时指定 CA
agent-ops-mcp --ca-cert ca.crt ...
```

