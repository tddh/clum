# clum 部署文档

> 最后更新：2026-08-14

## 架构（Central Server 模式）

```
┌─────────────────┐  HTTP :9788 (MCP)   ┌────────────────────────────────────────┐
│  AI 客户端        │◄──────────────────►│        Central MCP Server              │
│ (OpenCode/Claude) │                     │        clum-mcp --mode http        │
└─────────────────┘                     │                                        │
┌─────────────────┐  QUIC :9788         │  TCP :9788  HTTP/2 (MCP 生态)          │
│  人类运维         │◄──────────────────►│  UDP :9788  QUIC (Bridge/CLI)          │
│  (clum-cli)   │  PTY/push/forward  │                                        │
└─────────────────┘                     │  集中审计 + API Key + 注册表 + 静态文件  │
                                        └───────────┬──────────┬─────────────────┘
                                                    │ QUIC     │ QUIC
                                          ┌─────────▼──┐  ┌───▼──────────┐
                                          │ rmux-bridge │  │ rmux-bridge  │  ...
                                          │ (host-1)    │  │ (host-N)     │
                                          └──────┬──────┘  └──────┬───────┘
                                                 │ Unix Socket     │
                                          ┌──────▼──────┐  ┌──────▼───────┐
                                          │ RMUX daemon │  │ RMUX daemon  │
                                          └─────────────┘  └──────────────┘
```

- **clum-mcp (Central Server)**: 中央 MCP Server，双栈监听。AI 客户端通过 HTTP 连接，Bridge 通过 QUIC 反向注册。
- **clum-cli**: 命令行工具，通过 QUIC 连接 Server 中继到 Bridge。支持 term/push/pull/forward/list/replay；push/pull 支持文件与目录（1MB 分块流式传输，目录上传支持 `--exclude` 过滤）。
- **rmux-bridge**: 部署在每台目标 Linux 主机，主动连接 Server 注册，处理工具执行、文件 I/O、PTY、录制推送。
- **RMUX daemon**: 每个 Linux 主机上的终端多路复用器。

## 快速部署

### 1. 部署 Server

```bash
bash deploy/deploy-mcp.sh ./target/x86_64-unknown-linux-musl/release/clum-mcp root@<server-ip>
```

脚本自动完成：
- 上传二进制到 `/usr/local/bin/clum-mcp`
- 上传证书（ca.crt、server.crt、server.key）到 `/etc/clum/`
- 上传 `hosts.yaml` 到 `/etc/clum/`
- 首次部署时生成默认 `server-config.yaml`
- 创建 `clum-mcp.service` systemd 服务并启动

### 2. 添加 Bridge

```bash
# Server 侧生成 token
clum-mcp bridge add my-host --tags gpu,web
# 输出 token 和安装命令

# 目标机器一键安装
curl -fsSLk -H "Authorization: Bearer <download_token>" https://SERVER:9788/releases/install.sh | \
  BRIDGE_TOKEN=<token> SERVER_ADDR=SERVER:9788 sh
```

### 3. 配置 AI 客户端

```json
{
  "mcp": {
    "clum": {
      "type": "remote",
      "url": "https://SERVER:9788/mcp",
      "headers": { "Authorization": "Bearer yk_tddh_..." }
    }
  }
}
```

## 前置条件

| 组件 | 要求 |
|------|------|
| 目标主机 | Linux x86_64，systemd，有 SSH 访问 |
| RMUX | `rmux` 0.10.0 daemon 已安装并运行（`curl -fsSL https://rmux.io/install.sh \| sh`，版本由部署脚本固定） |
| 构建机 | Rust 1.85+，`x86_64-linux-musl-gcc`（交叉编译用 `brew install FiloSottile/musl-cross/musl-cross`） |
| 端口 | Server 监听 9788（TCP HTTP + UDP QUIC）；Bridge 为出站连接，无需开放入站端口 |
| 证书 | 自签名 TLS 证书（`openssl` 即可） |

## 快速开始

### 1. 构建

```bash
# 本机构建（macOS 开发）
cargo build -p clum-mcp --release
cargo build -p clum-cli --release

# 交叉编译 bridge（Linux x86_64，静态链接）
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc \
  cargo build --target x86_64-unknown-linux-musl --release -p rmux-bridge

# 或用 just 快捷命令
just release-linux          # 交叉编译 bridge + mcp（Linux 目标）
just build-mcp              # 本机构建 mcp
```

构建产物：
- `target/release/clum-mcp` — MCP server（本地运行）
- `target/release/clum-cli` — 命令行工具（本地运行）
- `target/x86_64-unknown-linux-musl/release/rmux-bridge` — bridge（部署到远程）

### 2. 部署

部署分两步：先部署 `rmux-daemon`，再部署 `rmux-bridge`。

**步骤 2a：部署 daemon**

```bash
bash deploy/install-daemon.sh root@<your-bridge-ip>
```

做的事：
- 安装 rmux（如未安装）
- 上传项目定制的 `rmux-daemon.service`（配置 `RMUX_TMPDIR=%h/.rmux`，启动参数 `--config-default --config-quiet` 自动启用 passthrough 等必要选项）
- 启动 daemon
- 写入 `/etc/profile.d/clum.sh`（`export RMUX_TMPDIR=$HOME/.rmux`），用户登录后可直接 `rmux a -t clum`

**步骤 2b：部署 bridge**

```bash
# 方式 1：Central Server 模式一键安装（推荐，bridge 主动注册到 Server）
curl -fsSLk -H "Authorization: Bearer <download_token>" \
  https://SERVER:9788/releases/install.sh | \
  BRIDGE_TOKEN=<token> SERVER_ADDR=SERVER:9788 sh

# 方式 2：手动部署
bash deploy/deploy-bridge.sh root@<your-bridge-ip>
```

部署脚本自动完成：
- 上传 `rmux-bridge` 二进制到 `/usr/local/bin/rmux-bridge`
- 下载 CA 证书到 `/etc/clum/ca.crt`
- 写入配置到 `/etc/clum/bridge.env`（权限 600）：`BRIDGE_AUTH_TOKEN`、`RMUX_SOCKET`（自动检测）、`RECORDING_ENABLED`、`RECORDING_DIR`、`BRIDGE_AUDIT_DB`、`CLUM_SERVER_ADDR`、`CLUM_CA_CERT`；direct 模式另含 `QUIC_LISTEN_ADDR`、`BRIDGE_TLS_CERT`、`BRIDGE_TLS_KEY`（enrolled 模式即配置了 `CLUM_SERVER_ADDR` 时不监听本地 QUIC 端口，无需这些参数，消除多余端口占用与攻击面）
- 创建 `rmux-bridge.service`（`systemctl enable --now`）

**其他 Justfile 命令：**

| 命令 | 说明 |
|------|------|
| `just certs` | 生成本地测试用自签名证书 |
| `just certs-host host=<name>` | 为指定主机生成 TLS 证书 |

**生成的 systemd 服务文件**（`/etc/systemd/system/rmux-bridge.service`）：

```ini
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
```

### 3. 更新 bridge（已有安装）

```bash
# 交叉编译
just release-linux

# 替换二进制 + 重启
ssh root@<your-bridge-ip> "systemctl stop rmux-bridge"
scp target/x86_64-unknown-linux-musl/release/rmux-bridge root@<your-bridge-ip>:/usr/local/bin/rmux-bridge
ssh root@<your-bridge-ip> "systemctl start rmux-bridge"

# 验证
ssh root@<your-bridge-ip> "systemctl status rmux-bridge --no-pager"
```

### 4. Bridge CLI 参数参考

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--quic-listen-addr` | `0.0.0.0:9778` | QUIC/UDP 监听地址（终端操作 + 文件传输，仅直连模式使用） |
| `--max-connections` | `256` | 最大并发连接数，0=无限制（`MAX_CONNECTIONS` 环境变量） |
| `--rmux-socket` | `/tmp/rmux-1000/default` | RMUX daemon Unix socket 路径 |
| `--tls-cert` | `certs/bridge.crt` | TLS 证书路径（CA 签发） |
| `--tls-key` | `certs/bridge.key` | TLS 私钥路径 |
| `--auth-token` | 环境变量 `BRIDGE_AUTH_TOKEN` | 认证令牌 |
| `--log-level` | `info` | 日志级别：trace/debug/info/warn/error（`RUST_LOG` 环境变量） |
| `--idle-timeout-secs` | `28800` | 交互式空闲超时（秒），超时后断连并恢复 pane 布局。0=禁用（`IDLE_TIMEOUT_SECS` 环境变量） |
| `--recording-enabled` | `true` | 启用 PTY 录制（`RECORDING_ENABLED` 环境变量） |
| `--recording-dir` | 自动检测 | 录制文件存储目录（`RECORDING_DIR` 环境变量） |
| `--recording-retention-days` | `90` | 录制保留天数（`RECORDING_RETENTION_DAYS` 环境变量） |
| `--recording-max-size-mb` | `500` | 录制容量上限 MB（`RECORDING_MAX_SIZE_MB` 环境变量） |
| `--recording-fsync-interval-secs` | `5` | 录制 fsync 间隔秒（`RECORDING_FSYNC_INTERVAL_SECS` 环境变量） |
| `--bridge-audit-db` | 自动检测 | Bridge 侧审计数据库路径（`BRIDGE_AUDIT_DB` 环境变量） |

> **QUIC 协议**：所有通信走 QUIC（UDP :9788），内置 TLS 1.3 加密。确保防火墙放行 UDP 9788（Server）和 9778（Bridge 直连回退）端口。

### 5. MCP Server CLI 参数参考

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--mode` | `stdio` | 运行模式：`stdio`（本地）或 `http`（Central Server） |
| `--config` | 无 | server-config.yaml 路径（http 模式推荐，YAML 配置覆盖 CLI 参数） |
| `--listen` | 无 | HTTP/QUIC 监听地址（http 模式，如 `0.0.0.0:9788`） |
| `--server-cert` | 无 | TLS 服务器证书路径（http 模式必填） |
| `--server-key` | 无 | TLS 服务器私钥路径（http 模式必填） |
| `--api-keys` | 无 | API Key 列表（逗号分隔，http 模式认证） |
| `--bridge` | 无 | Bridge token（`HOSTNAME=TOKEN` 格式，可多次指定） |
| `--static-dir` | 无 | 静态文件服务目录（install.sh、ca.crt、releases 等） |
| `--hosts-file` | `config/hosts.yaml` | 主机注册表路径（直连回退用） |
| `--ca-cert` | 无 | CA 证书路径（必填，不传则拒绝连接） |
| `--audit-db` | `~/.clum/audit.db` | 审计数据库路径 |
| `--audit-retention-days` | `90` | 审计数据保留天数 |
| `--audit-max-size-mb` | `500` | 审计数据库大小上限 (MB) |
| `--audit-cleanup-interval-secs` | `600` | 自动清理间隔（秒） |
| `--audit-sync-interval-secs` | `300` | 录制文件同步拉取间隔（秒） |
| `--recordings-dir` | `~/.clum/recordings` | 本地录制存储目录 |
| `--recordings-retention-days` | `90` | 本地录制保留天数 |
| `--recordings-max-size-mb` | `5000` | 本地录制容量上限 (MB) |

### 6. 认证模式

**AI 客户端认证（Central Server 模式）**：

API Key 格式 `yk_{name}_{32hex}`，SHA-256 哈希存储在 SQLite。通过 HTTP Bearer header 传递。

```bash
# Server 侧管理 API Key
clum-mcp agent add tddh --admin          # 创建超管 Key（需显式 --admin）
clum-mcp agent add alice --group infra   # 创建组内 Key（仅访问 infra 组主机）
clum-mcp agent list            # 列出（含 GROUP 列）
clum-mcp agent rotate tddh     # 轮换（继承原 group）
clum-mcp agent revoke tddh     # 吊销
```

**Group 隔离（RBAC）**：

| Key 类型 | 权限 |
|----------|------|
| 无 group（超管） | 访问所有主机、所有工具 |
| 有 group | 仅访问本组主机；`host_list`/`audit_query`/`list_recordings` 自动过滤；`reload_config`/`host_set_meta` 不可用 |

主机的 group 在 `hosts.yaml` 的 `group` 字段或 `bridge add --group` 时指定。组内 Key 看不到组外主机的存在。

**Bridge 认证**：

Bridge 使用静态 token 认证，通过常数时间比较（防时序攻击）。

```bash
# Central Server 模式：Server 侧生成 token
clum-mcp bridge add my-host --tags gpu,web

# 直连模式：hosts.yaml 配置
# config/hosts.yaml
hosts:
  - name: tf01
    bridge_addr: 10.0.1.10:9778
    bridge_token: "your-secure-token"
```

`bridge_token` 和 Bridge 端环境变量 `BRIDGE_AUTH_TOKEN` 中的 token 必须一致。

### 7. 配置主机注册表

创建 `config/hosts.yaml`：

```yaml
hosts:
  - name: tf01                         # MCP 工具中引用的主机名
    bridge_addr: <your-bridge-ip>:9778     # bridge 直连地址（仅 direct 模式需要）
    bridge_token: "<your-token>"              # 认证 token（仅 direct 模式需要）
    group: production                  # 分组（host_filter 用）
    tags: [web, nginx]                  # 标签
    labels:                             # 键值对标签
      dc: shanghai
      rack: a3
    # 可选：限制隧道目标（不配置 = 全部允许）
    # allowed_forward_targets:
    #   - "127.0.0.1:5432"             # 精确匹配
    #   - "10.0.1.*:*"                 # glob 通配符
    #   - "*:3306"                     # 所有主机的 MySQL
```

> 💡 **热加载**：修改 `hosts.yaml` 后无需重启 MCP Server — 调用 `reload_config` MCP 工具或向进程发送 `kill -HUP <pid>` 即可生效。加载失败时保留原有配置，不影响运行中服务。

### 8. 配置 AI 客户端

**Central Server 模式**（推荐）：

```json
{
  "mcp": {
    "clum": {
      "type": "remote",
      "url": "https://SERVER:9788/mcp",
      "headers": { "Authorization": "Bearer yk_name_..." }
    }
  }
}
```

**本地 stdio 模式**（开发/测试）：

```json
{
  "mcp": {
    "clum": {
      "type": "local",
      "command": ["/path/to/clum-mcp"],
      "args": ["--hosts-file", "config/hosts.yaml", "--ca-cert", "certs/ca.crt"],
      "enabled": true
    }
  }
}
```

### 9. 验证

```bash
# 直接调 MCP 测试
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"host_list","arguments":{}}}' \
  | target/release/clum-mcp --hosts-file config/hosts.test.yaml --ca-cert /tmp/bridge-remote.crt 2>/dev/null
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
clum-mcp audit query --format table

# 查特定主机的命令执行记录
clum-mcp audit query --host tf01 --action exec --since 2026-06-01

# 统计概览
clum-mcp audit stats

# 手动清理
clum-mcp audit cleanup --older-than 30
```

审计数据默认存储在 `~/.clum/audit.db`，保留 90 天，上限 500MB。

## 目录结构

```
~/.clum/                      # MCP Server 本地
├── audit.db                       # 审计数据库（SQLite）
└── recordings/                    # PTY 录制文件（Bridge 实时推送 + 定期同步）

/usr/local/bin/
└── rmux-bridge                   # bridge 二进制

/etc/clum/                    # 远程主机配置
├── bridge.env                    # BRIDGE_AUTH_TOKEN + SERVER_ADDR + CA 路径（权限 600）
├── ca.crt                        # CA 根证书
├── bridge.crt                    # 主机 TLS 证书（可选，直连回退用）
└── bridge.key                    # TLS 私钥（权限 600，可选）

/opt/clum/                       # 远程主机数据
├── recordings/                   # PTY 录制文件（asciinema v2）
└── bridge_events.db              # Bridge 侧审计数据库

/etc/systemd/system/
├── rmux-daemon.service           # daemon systemd 服务
└── rmux-bridge.service           # bridge systemd 服务

/etc/profile.d/
└── clum.sh                  # RMUX_TMPDIR 环境变量
```

## 故障排查

| 症状 | 检查 |
|------|------|
| CLI `term` 后按键无效、终端卡死 | rmux 0.9 将 `allow-passthrough` 默认改为 `off`。项目 `rmux-daemon.service` 通过 `--config-default` 自动启用 passthrough。若使用自定义 service，确认启动参数包含 `--config-default` 或手动 `rmux set -g allow-passthrough on`，然后 `systemctl restart rmux-daemon`。 |
| MCP 工具返回 `connection refused` | `systemctl status rmux-bridge`，确认 bridge 在运行 |
| `authentication failed` | 检查 `bridge.env` 中的 `BRIDGE_AUTH_TOKEN` 与 `hosts.yaml` 中 `bridge_token` 是否一致 |
| TLS 握手失败 | `--ca-cert` 指向的证书是否与 bridge 端一致 |
| `unknown request type` | bridge 版本过旧，重新交叉编译部署 |
| RMUX socket 找不到 | `ls $HOME/.rmux/rmux-*/default`，确认 rmux daemon 在运行（socket 路径由 `RMUX_TMPDIR` 环境变量控制，项目 daemon service 配置为 `$HOME/.rmux`，部署脚本自动检测实际路径，未检测到时回退标准路径 `/root/.rmux/rmux-0/default`） |

## 安全

### Unix Socket

rmux daemon 的 socket 路径由 `RMUX_TMPDIR` 环境变量控制。项目定制的 `rmux-daemon.service` 设置 `RMUX_TMPDIR=%h/.rmux`（root 用户展开为 `/root/.rmux`），socket 位于 `$RMUX_TMPDIR/rmux-<UID>/default`。

部署脚本自动检测实际 socket 路径，无需手动指定（未检测到时回退标准路径 `/root/.rmux/rmux-0/default`，与项目 `rmux-daemon.service` 布局一致）。Socket 权限为 `srw-------`（仅 owner 可读写），其他用户无法访问。

如果需要在自定义路径运行 rmux daemon，同步更新：
- `rmux-daemon.service` 中的 `Environment=RMUX_TMPDIR=...`
- `/etc/profile.d/clum.sh` 中的 `export RMUX_TMPDIR=...`
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
  -subj "/CN=clum-ca" -addext "basicConstraints=critical,CA:TRUE"

# 为 bridge 签发（替换 <your-bridge-ip> 为实际 IP）
openssl req -new -newkey rsa:2048 -keyout bridge.key -out bridge.csr -nodes \
  -subj "/CN=<your-bridge-ip>" -addext "subjectAltName=DNS:<your-bridge-ip>,IP:<your-bridge-ip>"
openssl x509 -req -in bridge.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out bridge.crt -days 365

# MCP server 启动时指定 CA
clum-mcp --ca-cert ca.crt ...
```

