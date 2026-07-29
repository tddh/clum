# yunying 中央服务器架构设计方案

> 版本：v1.0 draft | 日期：2026-07-29 | 状态：设计中

## 1. 背景与动机

### 1.1 当前架构（Sidecar 模式）

```
[AI Client A] ←stdio→ [MCP-A 本地进程] ←QUIC 主动连→ [Bridge-1]
[AI Client B] ←stdio→ [MCP-B 本地进程] ←QUIC 主动连→ [Bridge-2]
[Human]       ←PTY──→ [yunying-cli]    ←QUIC 主动连→ [Bridge-3]
```

每个 AI 客户端需要独立部署 MCP 二进制 + `hosts.yaml` + CA 证书。

### 1.2 痛点

| 痛点    | 说明                                                              |
| ----- | --------------------------------------------------------------- |
| 部署分散  | 每个 AI 客户端都要装 MCP + 配置                                           |
| 配置静态  | `hosts.yaml` 手动维护，Bridge 地址变更需改配置                               |
| 审计分散  | 审计 DB 在各客户端，无法集中查看                                              |
| 无连接复用 | Sidecar 模式下每次工具调用新建 QUIC 连接（Hub 模式用长连接 + stream 复用解决） |
| 会话发现难 | Sidecar 模式下各 MCP 实例互相不知道对方的 session（rmux 本身支持多客户端共享，Hub 模式自然解决） |

### 1.3 目标架构（Hub 模式，双栈）

```
                        ┌──────────────────────────────────────┐
                        │          Central MCP Server            │
                        │                                        │
  AI Agent ──TCP───────→│  TCP :9778  HTTP/2                     │
  (opencode/Claude/     │          rmcp StreamableHttpService    │
   Cursor/Codex)        │          MCP 标准 Streamable HTTP      │
                        │                                        │
  yunying-cli ──QUIC───→│  UDP :9778 QUIC                       │
  自建 Agent ──QUIC────→│          ALPN 协商：                   │
                        │            "h3"      → HTTP/3 (可选)   │
  Bridge-1 ──QUIC──────→│            "yunying" → 原生帧协议      │
  Bridge-2 ──QUIC──────→│                                        │
                        │  共享：工具执行层 + 注册表 + 审计 DB     │
                        └──────────────────────────────────────┘
```

**双栈：TCP 给 MCP 生态，QUIC 给自己的组件。**

- **TCP :9778**（HTTP/2）：opencode、Claude Desktop、Cursor、Codex 等标准 MCP 客户端。rmcp `StreamableHttpService` 直接支持。
- **UDP :9778**（QUIC）：Bridge 反向注册 + yunying CLI PTY 透传 + 自建 Agent。ALPN 区分 `h3`（HTTP/3，可选）和 `yunying`（原生帧协议）。
- 同一端口号，TCP/UDP 互不干扰。两个入口，一套工具执行逻辑。

### 1.4 范围界定

**本期只做中央化。** 以下明确不做：

| 不做            | 原因                 |
| ------------- | ------------------ |
| 权限/角色模型（RBAC） | 运维工具的权限模型特殊，需要单独设计 |
| 主机级访问控制       | 同上                 |
| 命令策略引擎        | 同上                 |
| 速率限制          | 内部工具，暂不需要          |
| OAuth2 / OIDC | 企业级需求，验证 PMF 后再做   |
| 审批流           | 运维工具不适合审批          |
| 高可用 / 集群      | 单点先跑起来             |

**Agent 认证 = 有 API Key 就通过，持有有效 Key 即拥有全部权限。** Key 的作用是标识身份（审计追溯），不是授权。

---

## 2. 协议与传输

### 2.1 MCP 协议版本

| 项目 | 当前 | 目标 |
|------|------|------|
| 协议版本 | `2024-11-05` | `2026-07-28` |
| 传输 | stdio（手写 JSON-RPC） | Streamable HTTP（rmcp v3.0.0） |
| 握手 | `initialize` | 仅 `server/discover`（不兼容旧协议） |
| 会话 | 无 | 无状态（2026-07-28 移除了 Mcp-Session-Id） |

### 2.2 传输层设计（双栈）

| 端口 | 协议 | 使用者 | 说明 |
|------|------|--------|------|
| **TCP :9778** | HTTP/2 | opencode / Claude / Cursor / Codex / 标准 MCP SDK | rmcp `StreamableHttpService` 直接支持 |
| **UDP :9778** | QUIC（ALPN 路由） | Bridge / yunying-cli / 自建 Agent | 长连接、多路复用、连接迁移 |

QUIC 端口 ALPN 协商：

| ALPN | 协议 | 使用者 |
|------|------|--------|
| `h3` | HTTP/3 Streamable HTTP | 支持 HTTP/3 的 MCP 客户端（可选，未来） |
| `yunying` | 原生 QUIC 帧协议 | Bridge 注册 / CLI PTY / 自建 Agent |

```rust
// TCP 栈：rmcp 直接支持
let http_service = StreamableHttpService::new(tool_handler, config);
axum::serve(tcp_listener, http_service.into_make_service()).await?;

// QUIC 栈：ALPN 路由
let mut server_config = quinn::ServerConfig::with_crypto(tls_config);
server_config.alpn_protocols(&[b"h3", b"yunying"]);

while let Some(connecting) = endpoint.accept().await {
    let conn = connecting.await?;
    match conn.alpn().unwrap_or_default() {
        b"h3"      => handle_http3(conn),
        b"yunying" => handle_native_quic(conn),
        _          => conn.close(0, b"unsupported"),
    }
}
```

### 2.3 文件传输与隧道（数据平面）

文件传输和隧道是**数据平面**操作。Agent 通过 skill 知道如何调用 CLI，CLI 连接 Server QUIC 端口，Server 中继到 Bridge：

```
控制平面（MCP 工具调用）：
  Agent → MCP tools/call exec → TCP :9778 → Server → Bridge

数据平面（CLI 命令，Agent 通过 Bash 工具执行）：
  Agent → Bash("yunying-cli upload tf01 ./a.conf /etc/a.conf")
        → CLI 进程 → QUIC :9778 → Server 流式中继 → Bridge → Linux 磁盘
```

**Server 仍然做流式中继**（文件 1MB 分块，隧道 64KB 分块，不缓冲整文件）。区别只是 Agent 侧的触发方式：通过 skill 指导 Agent 调用 CLI，而非直接调 MCP 工具。

```bash
# 文件传输（Agent 通过 skill 知道这些命令）
yunying-cli upload <host> <local_path> <remote_path>
yunying-cli download <host> <remote_path> <local_path>

# 隧道
yunying-cli tunnel <host> --local <port> --remote <host:port>

# PTY 透传
yunying-cli connect <host> [--session <name>]
```

MCP 侧的 `file_upload`/`file_download`/`tunnel_create` 工具保留（兼容直接调 MCP 的场景），底层走同一条 QUIC 流式通道。

CLI 用法写入 `.opencode/skills/` 或 `AGENTS.md`，让 Agent 知道怎么调用。

#### 隧道具体方案

```
yunying-cli tunnel tf01 --local 5432 --remote 127.0.0.1:5432

数据路径（每个入站连接）：
  本地应用 → localhost:5432 (CLI TCP 监听)
           → QUIC stream → Server (透传)
           → QUIC stream → Bridge
           → TCP connect → 127.0.0.1:5432 (远程主机上的服务)
           → 双向字节流透传
```

- CLI 在本地监听 TCP 端口，每个入站连接开一条 QUIC stream
- Server 不解析内容，双向透传字节
- Bridge 收到 stream 后做 TCP connect 到目标地址
- 连接关闭时整条链路释放

#### CLI connect（PTY 透传）具体方案

```
yunying-cli connect tf01 [--session yunying]

数据路径：
  用户终端 (raw mode)
    ↔ CLI (crossterm raw mode, 本地渲染)
    ↔ QUIC stream (双向)
    ↔ Server (透传，不解析 PTY 内容)
    ↔ QUIC stream (双向)
    ↔ Bridge
    ↔ unix socket ↔ rmux daemon ↔ PTY
```

- Server 是透明字节中继，不缓冲、不解析
- CLI 负责本地终端 raw mode 切换、窗口大小同步（TIOCSWINSZ）
- 断连时 CLI 提示重连，rmux session 不丢失（持久化在 Bridge 侧）
- 多客户端可同时 connect 同一个 session（rmux 天然支持）

### 2.4 会话共享与并发

rmux 本身支持多客户端 attach 到同一个 session。Sidecar 模式下"会话不共享"是因为各 MCP 实例互相不知道对方的 session，Hub 模式自然解决（所有 Agent 通过同一个 Server 路由到同一个 Bridge）。

**并发不需要额外控制机制**：

| 场景 | 为什么不冲突 |
|------|-------------|
| 两个 Agent 同时 exec 同一个 pane | exec 有 sentinel 机制，各自等自己的标记 |
| Agent-A 在 vim 里，Agent-B 发 exec | exec 安全门：非 ready 状态拒绝执行 |
| capture_pane 时另一个 Agent 在操作 | capture 是快照语义，拿到什么就是什么 |
| 两个 Agent 同时 file_upload 同一路径 | 后写的赢，和两个人同时 scp 一样 |

最坏情况 = 两个人同时操作同一个终端，是运维场景的正常情况。

### 2.5 MCP SDK 策略

| 角色 | SDK | 版本 | 说明 |
|------|-----|------|------|
| Server | rmcp (Rust) | **v3.0.0** | 2026-07-28 spec，stateless Streamable HTTP，Transport trait 可插拔 |
| Python Client | mcp (官方) | v2 | 用户直接用，不需要我们提供 |
| TypeScript Client | @modelcontextprotocol/client | v2 | 同上 |
| Go Client | go-sdk (官方) | v1.7+ | 同上 |
| yunying CLI | 自有 QUIC 协议 | — | 通过 rmcp Transport trait 适配 |

---

## 3. Bridge 反向注册

### 3.1 节点标识模型

**Token 是节点身份锚，hostname 是显示名。**

- Bridge 注册时**不自报 hostname**，Server 从 token 绑定关系中取，防止伪造
- tags/labels 由 Server 定义，绑定在 token 上，Bridge 不能自报
- `machine_id`（`/etc/machine-id`）作为辅助校验：首次注册记录，后续变化触发审计告警
- hostname 变更是管理员操作：`bridge update tf01 --hostname tf01-new`

### 3.2 注册流程

```
Bridge 启动
  → QUIC 连到 Server :9778（ALPN "yunying"，0-RTT 如果之前连过）
  → 发送注册消息（不含 hostname）：
    {
      "type": "bridge_register",
      "token": "64位hex",
      "version": "0.9.0",
      "capabilities": ["exec", "file", "tunnel", "interactive"],
      "machine_id": "a1b2c3d4...",
      "os_info": "Ubuntu 24.04 x86_64"
    }
  → Server 验证 token（SHA-256 哈希 + constant_time_eq）
  → Server 从 bridges 表取 hostname/tags/labels
  → 检查 machine_id 一致性（不一致 → 审计告警，不拒绝）
  → 注册成功：存入内存连接注册表，返回确认
  → 注册失败：返回拒绝原因，Bridge 指数退避重连
```

### 3.3 Server 侧 Bridge 存储

```sql
CREATE TABLE bridges (
    token_hash          TEXT PRIMARY KEY,
    token_prefix        TEXT NOT NULL,
    hostname            TEXT NOT NULL UNIQUE,
    tags                TEXT NOT NULL DEFAULT '[]',
    labels              TEXT NOT NULL DEFAULT '{}',
    machine_id          TEXT,
    os_info             TEXT,
    rotated_at          TEXT NOT NULL,
    previous_token_hash TEXT,
    previous_expires_at TEXT,
    pending_rotation    INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL,
    revoked_at          TEXT
);
```

### 3.4 Bridge 侧配置与本地状态

```
/etc/yunying/
├── ca.crt          ← CA 证书（可选，私有 CA 时需要）
├── token           ← 最新 token（轮换后写入，权限 0600）
└── bridge.yaml     ← 可选配置（server_addr 等）

/etc/systemd/system/rmux-bridge.service
└── Environment=BRIDGE_AUTH_TOKEN=初始token   ← 仅首次安装用
```

**Token 来源优先级**：
1. `/etc/yunying/token` 文件（轮换后写入的最新 token，优先）
2. `BRIDGE_AUTH_TOKEN` 环境变量（首次安装时 systemd unit 里的，回退）

**TLS 验证（强制，无 skip-verify）**：

| Server 证书类型 | Bridge 验证方式 | 需要配置 CA？ |
|----------------|----------------|:------------:|
| 公有 CA（Let's Encrypt 等） | 系统内置信任库 | ❌ |
| 私有 CA（企业自建） | 指定 CA 根证书文件 | ✅ |

### 3.5 Token 自动轮换（24h TTL）

QUIC/TLS 1.3 已提供传输加密和服务端认证。Token 仅用于"授权注册"（敲门），不提供持续安全。

```
Token TTL：  24 小时
过渡期：    旧 token 在新 token ACK 后额外 24h 有效（共 48h 窗口）
轮换方式：  Server 通过 QUIC 控制流推送，Bridge 持久化到 /etc/yunying/token
离线保护：  Bridge 离线 > 48h → token 过期 → 需管理员生成新 join token
```

正常情况永远不需要人工介入。Bridge 离线超 48h 才需要管理员执行 `bridge join tf01`。

### 3.6 首次安装与引导

首次安装需要一次带外访问（SSH / cloud-init）。安装完成后所有更新走 QUIC 通道。

```bash
# yunying-server bridge add tf01 --tags gpu,training 输出：
# 安装命令（在目标机器上执行）：
curl -fsSL https://10.0.0.1:9778/install.sh | \
  BRIDGE_TOKEN=8f3a... \
  SERVER_ADDR=10.0.0.1:9778 \
  sh
```

Server 在 TCP :9778 端口通过 HTTP/2 提供静态文件（install.sh、二进制、CA 证书）。

后续更新（全自动，无需 SSH）：

| 操作 | 方式 |
|------|------|
| Bridge 二进制更新 | Server 通过 QUIC 推送 |
| Token 轮换 | Server 通过 QUIC 推送（24h 自动） |
| CA 证书更新 | Server 通过 QUIC 推送 |

### 3.7 连接模型

- **一条连接 = 一个 Bridge**，QUIC 多路复用
- **心跳**：QUIC 层 keepalive 15s，应用层 60s 超时标记 offline
- **重连**：Bridge 侧指数退避（500ms → 30s）
- **规模**：1000 个 Bridge ≈ 100MB 内存

### 3.8 请求路由

```
Agent 调用 exec host=tf01
  → Server 查注册表：tf01 → BridgeConn
  → 在已有 QUIC 连接上开新 stream
  → 发送 exec 请求，等响应
  → stream 关闭，连接继续活着
```

### 3.9 注册管理命令

```bash
yunying-server bridge add tf01 --tags gpu,training --labels dc=shanghai
yunying-server bridge list
yunying-server bridge update tf01 --hostname tf01-new
yunying-server bridge join tf01          # 生成新 join token（离线恢复用）
yunying-server bridge remove db-01
yunying-server bridge upgrade --all --version 0.9.0
yunying-server release upload --version 0.9.0 --linux-x86_64 ./rmux-bridge
```

---

## 4. Agent 认证

### 4.1 模型

**有 API Key 就通过，持有有效 Key 即拥有全部 66 个工具的权限。**

Key 的作用是**标识身份**（审计追溯"谁做了什么"），不是授权。

```
Agent 请求 → 验证 API Key → 有效 → 执行（全部权限）
                         → 无效/过期/吊销 → 401
```

### 4.2 API Key 格式与存储

```
yk_{name}_{32字节hex}

示例：yk_tddh_a1b2c3d4e5f6...
      │  │    │
      │  │    └─ 32 字节随机数（CSPRNG，SHA-256 后存储）
      │  └─ 用户标识（管理员创建时指定，字母/数字/短横线）
      └─ 固定前缀
```

**名字嵌在 key 里，key 就是身份。** 不管用户用什么客户端（opencode/Cursor/curl），key 里就写了是谁。审计日志直接从 key 提取 name。

```sql
CREATE TABLE api_keys (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,        -- "tddh"（从 key 中提取，冗余存储便于查询）
    key_hash     TEXT NOT NULL,        -- SHA-256(完整 key)
    key_prefix   TEXT NOT NULL,        -- "yk_tddh_a1b2"（日志用）
    created_at   TEXT NOT NULL,
    expires_at   TEXT,                 -- NULL = 永不过期
    revoked_at   TEXT,                 -- NULL = 未吊销
    last_used_at TEXT
);
```

### 4.3 认证方式

**TCP（HTTP）客户端**：
```http
POST /mcp HTTP/2
Authorization: Bearer yk_tddh_a1b2c3d4...
```

**QUIC（原生帧）客户端**：
```json
{"type": "agent_connect", "api_key": "yk_tddh_a1b2c3d4...", "client_info": {"name": "opencode"}}
```

### 4.4 管理命令

```bash
yunying-server agent add tddh
# API Key: yk_tddh_a1b2c3d4e5f6...（只展示一次）

yunying-server agent list
# NAME     KEY PREFIX        CREATED      LAST USED
# tddh     yk_tddh_a1b2     2026-07-29   2026-07-29 10:30
# ci-bot   yk_ci-bot_c3d4   2026-07-20   2026-07-29 09:15

yunying-server agent rotate tddh        # 旧 key 24h 后失效
yunying-server agent revoke ci-bot      # 立即失效
```

### 4.5 安全兜底（不依赖权限系统）

| 机制 | 说明 |
|------|------|
| Exec 安全门 | Bridge 侧，非 ready 状态拒绝执行 |
| 路径穿越防护 | 拒绝 `..` 和 null byte |
| 全链路审计 | 所有操作可追溯（谁、什么时间、哪台机器、什么命令） |
| API Key 吊销 | 发现异常立即切断 |
| TLS 1.3 强制 | 传输加密，无 skip-verify |

---

## 5. 审计

审计全部在 Server 侧记录，Bridge 侧不再写 SQLite。

| 事件 | 记录内容 |
|------|----------|
| `agent_auth_success/failure` | agent name, IP, timestamp |
| `bridge_register/deregister` | hostname, IP, version, timestamp |
| `token_rotated` | hostname, timestamp |
| 工具调用（exec 等） | agent, host, tool, detail, success, duration（复用现有） |
| `tunnel_start` | agent, host, local_port, remote_host:port, timestamp |
| `tunnel_end` | agent, host, local_port, duration, bytes_transferred, timestamp |
| `file_transfer` | agent, host, direction(upload/download), local_path, remote_path, size, sha256, timestamp |

**隧道不记录内容**——可能走任意协议（TCP/UDP/自定义），无法全部兼容，只记录开始/结束和流量统计。

**文件传输记录元数据**——方向、源路径、目标路径、操作 agent、大小、校验和。不记录文件内容。

**存储位置**：默认 `~/.yunying/audit.db`，可通过 `--audit-db` 参数或 `server-config.yaml` 中 `audit_db` 字段指定路径。

---

## 6. 现有功能影响

### 6.1 不受影响（~70% 代码量）

66 个工具的业务逻辑（`tools/*.rs`）、`schema.rs`、`error.rs`、审计逻辑。

### 6.2 需要重写（~15%）

| 模块             | 改动                                             |
| -------------- | ---------------------------------------------- |
| `transport.rs` | MCP 主动连 Bridge → Server 监听 + Bridge 反向连入 + 连接注册表 |
| `router.rs`    | 静态 hosts.yaml → 动态注册表                          |
| `main.rs`      | stdio 循环 → 双栈监听（TCP :9778 + QUIC :9778）+ 注册表    |
| `handler.rs`   | 手写 JSON-RPC → rmcp v3.0.0                      |

### 6.3 需要适配（~15%）

| 模块 | 改动 | 优先级 |
|------|------|:------:|
| `stream.rs` | Server 持有 stream，转发给 agent（SSE 或帧） | Phase 1 |
| `batch.rs` | 从连接注册表取多个 BridgeConn 并发（更简单了） | Phase 1 |
| `files.rs` | Agent/CLI → Server → Bridge 流式中继 | Phase 1 |
| `deploy.rs` | 通过 Server 路由 | Phase 1 |
| `progress.rs` | 流式推送（SSE 或帧） | Phase 1 |
| `recording_sync.rs` | Bridge 录制结束后主动推送到 Server（不拉） | Phase 1 |
| `tunnel.rs` | CLI 本地监听 → QUIC → Server → QUIC → Bridge → TCP 目标 | Phase 1 |
| CLI `connect` | CLI ↔ QUIC ↔ Server（透传）↔ QUIC ↔ Bridge ↔ rmux PTY | Phase 1 |
| Bridge 审计 | **移除** Bridge 侧 SQLite，审计全部在 Server 侧记录 | Phase 1 |

### 6.4 配置变化

| 当前 | 改为 |
|------|------|
| 客户端 `hosts.yaml` | **移除**。客户端只需 Server 地址 + API Key |
| Bridge `BRIDGE_AUTH_TOKEN` | **保留** |
| — | **新增** Server 侧 `server-config.yaml` |

---

## 7. 分阶段实施

### Phase 0：代码清理（1-2 天）

- [ ] `cargo fmt --all` 修复格式问题
- [ ] TOOLS.md "66 种 AuditAction" → "65 种"
- [ ] CHANGELOG 补充 `2d703a9` 提交记录
- [ ] 引入 rmcp v3.0.0 依赖，验证编译通过

### Phase 1：中央服务器 MVP（4-5 周）

- [ ] rmcp v3.0.0 集成（MCP 协议层，2026-07-28 stateless）
- [ ] Bridge 反向注册 + 连接注册表
- [ ] 双栈监听：TCP :9778（HTTP/2，rmcp StreamableHttpService）+ UDP :9778（QUIC ALPN）
- [ ] Server 配置（server-config.yaml）
- [ ] Agent API Key 认证（有 key 就过，标识身份）
- [ ] 请求路由（agent → Server → Bridge）
- [ ] 集中审计 DB（Server 侧，移除 Bridge 侧 SQLite）
- [ ] exec/session/pane/window/batch 工具通过 Server 路由
- [ ] stream_pane / wait_for_text 流式转发
- [ ] file_upload / file_download 流式中继（CLI + MCP 工具共享通道）
- [ ] 隧道转发（CLI 本地监听 → QUIC → Server → QUIC → Bridge → TCP 目标）
- [ ] CLI connect PTY 透传（CLI ↔ QUIC ↔ Server 透传 ↔ QUIC ↔ Bridge ↔ rmux PTY）
- [ ] 录制推送（Bridge 录制结束后主动推送到 Server）
- [ ] 进度通知流式推送
- [ ] Bridge token 自动轮换（24h TTL）
- [ ] install.sh + Server 静态文件服务
- [ ] CLI 用法写入 skill（让 Agent 知道怎么调 upload/download/tunnel/connect）

### Phase 2：增强

- [ ] mTLS Bridge 客户端证书
- [ ] 权限模型设计（单独设计，见第 10 节说明）
- [ ] HTTP/3 支持（QUIC 端口 ALPN h3）

### Phase 3：企业级（验证 PMF 之后）

- [ ] OAuth2 / OIDC 集成
- [ ] 合规报告导出
- [ ] 审计日志外部推送（Syslog / Webhook）
- [ ] 高可用（主备 / 集群）
- [ ] Prometheus / OpenTelemetry 指标

---

## 8. 技术选型

| 组件 | 选型 | 理由 |
|------|------|------|
| MCP 生态入口 | **TCP :9778 (axum + rmcp)** | HTTP/2，opencode/Claude/Cursor 直接连 |
| Bridge/CLI 入口 | **UDP :9778 (quinn)** | QUIC 长连接、多路复用、连接迁移 |
| MCP 协议层 | **rmcp v3.0.0** | 2026-07-28 spec，stateless HTTP，Transport trait 可插拔 |
| TLS | rustls | 已有，无 OpenSSL 依赖，TLS 1.3 |
| 审计存储 | rusqlite (SQLite) | 已有，集中到 Server 侧 |
| 配置格式 | YAML (serde_yml) | 已有 |

---

## 9. 风险与应对

| 风险 | 严重度 | 应对 |
|------|:------:|------|
| Server 单点故障 | 🔴 | Bridge 自动重连；exec sentinel 保证命令不丢；Phase 3 做主备 |
| rmcp v3.0 刚发布 | 🟡 | 锁定版本 `rmcp = "=3.0.0"`，跟踪 conformance #977 |
| 改造期间功能回退 | 🟡 | 保留 stdio 模式作为 fallback |
| 隧道/PTY 转发链路长 | 🟡 | 全在 Phase 1，需充分测试断连恢复和背压传导 |
| 文件传输中继 | 🟡 | 流式转发（1MB 分块），背压自动传导 |

---

## 10. 权限模型（暂不实施，留作参考）

**本期不做。** 原因：

yunying 是运维操作工具，权限模型和常规 API 不同：
- 常规 API：权限 = 谁能调哪个 endpoint（静态、可枚举）
- yunying：权限 = 谁能在这台机器上执行这条命令（动态、组合爆炸）
- 简单 RBAC 粒度太粗；命令级策略维护成本高
- 需要想清楚再动手

**未来方向**（仅供参考，不承诺实施）：
- 角色模型（admin/operator/readonly）
- 主机级访问控制（glob 匹配）
- 命令策略引擎（deny/allow 规则）
- OAuth2/OIDC 对接企业 IdP

**当前安全兜底**（已足够）：
- Bridge 侧 exec 安全门
- 路径穿越防护
- 全链路审计
- API Key 吊销
- TLS 1.3 强制
