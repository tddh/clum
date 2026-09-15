# 录制加密方案（完整实施版 · v2.2）

> 状态：**已实施** · 2026-09-11
> 目标：bridge 侧不留明文录制；全链路（bridge 磁盘 → 传输 → server 磁盘 → 回放端）无明文；搜索与回放照常可用。
> v2 修订：纳入代码级评审（Oracle + 主会话核对）发现的 2 项 CRITICAL + 8 项问题，全部修正。
> v2.1 修订：第二轮评审发现 v2 自身引入的 1 项致命（nonce 前缀未入头）+ 7 项定义未闭合，全部补齐。
> v2.2 修订：**实现落地后回写**——补密文头 `name` 字段、AAD 改为头内 `name`、keyring 实际结构、接线链实际路径（经 0x07 数据流，非 proxy）、公钥缺失降级为明文（非缓冲），并补实施/部署记录（§11）。

---

## 0. 一句话

**bridge 录制时就加密（X25519 信封 + 分块流式 AEAD），全链路只搬密文；server 存密文，搜索/回放时在内存里解密；回放端不落盘。** 明文只活在内存一瞬间。

---

## 1. 决策记录（全部已定）

| 项 | 结论 |
|----|------|
| 加密粒度 | **整文件加密**（不做识别） |
| 算法 | **单对 X25519** + AES-256-GCM（信封加密） |
| 公钥分发 | **走 TLS 通道下发**（注册时 server 发，bridge 内存持有，不落盘） |
| 解密位置 | **只在 server**（私钥唯一持有者） |
| server 落盘 | **密文（B2，用时解密）** |
| 搜索 | MCP `search_recordings`，**按个解密** |
| 回放 | cli **memfd 匿名文件**（Linux）/ 临时文件降级（macOS），**每次重新下载** |
| push 后删除 | **不做** |
| server 备份 | **不做**（本地即可） |
| 密码脱敏 | **不做** |

---

## 2. 总架构

```
┌─────────── bridge（每台主机）───────────┐
│  录制流(明文, 内存)                       │
│      │                                    │
│      ▼  [X25519 信封 + 分块 AES-256-GCM]  │
│  密文写盘  ← bridge 磁盘永远只有密文      │
└───────────────┬───────────────────────────┘
                │ push 30s / pull 300s（密文）
                ▼
┌─────────── server（clum-mcp）────────────┐
│  密文落盘（0600）+ fsync                   │
│      │                                    │
│      ├─ 格式嗅探 → 明文/密文都支持         │
│      ├─ 搜索：逐文件解密(内存)→扫描→丢明文 │
│      └─ 回放：解密(内存)→HTTPS→cli         │
│  私钥在此（唯一解密点）                    │
└───────────────┬───────────────────────────┘
                │ 公钥经 TLS 通道下发（注册时）
                ▼
┌─────────── clum-cli ─────────────────────┐
│  回放：下载→memfd(匿名内存文件)→播放→回收  │
│  每次重放重新下载；cli 不持密钥            │
└───────────────────────────────────────────┘
```

---

## 3. 详细设计

### 3.1 密钥生成与存储（server 侧 · keyring）

> **v2 修正（原 M6）**：单一 `recording.key` 替换即销毁旧私钥，会让 90 天保留期内的旧密文全部变盲区。改为**版本化 keyring**。

- server 维护 keyring 目录（默认 `{data_dir}/recording-keys`）。**current 密钥固定存 `current.key`**；轮换密钥为目录下任意 `*.key` 文件（**文件名不参与索引**）。
- `key_id` = 公钥的 SHA-256 指纹前 16 hex（公钥可推导，文件名无需等于 key_id）。
- server 启动时：`load_or_create`——**`current.key` 不存在就生成一对并写盘**（目录 0700、文件 0600）；随后扫描目录内所有 `*.key`，按各自 `key_id` 建索引，再确保 current 入表。
- **下发的是 current 公钥**；密文头记录 `key_id`；解密时按 `key_id` 找对应私钥。
- **轮换 = 追加**：生成新密钥 → 设为 current；**旧私钥保留至其密文全部过期（≥90 天 + 余量）**，绝不删除。
- **私钥离线备份**（物理隔离）——丢失 = 对应密文永久不可读。
- 配置项（server-config.yaml）：
  ```yaml
  recording_keys_dir: "/root/.clum/recording-keys"   # keyring 目录（目录 0700、密钥文件 0600）
  ```

### 3.2 公钥下发（注册协议扩展 + 完整接线链）

> **v2 修正（原 M5）**：原文档只列 `cast_recorder.rs` + `register.rs`，实际公钥要从注册响应一路透传到 writer_task，跨 **5 个文件 / 2 个调用点**，否则录制全部 fail-closed。

**协议**（`register_ack` 成功时，`quic_server.rs:handle_bridge_registration`）：
```json
{ "type": "register_ack", "ok": true,
  "recording_pubkey": "<base64 X25519 公钥>",
  "recording_key_id": "<16-hex 指纹>" }
```

**接线链（必须全改，缺一处即断；行号为落地版本）**：
```
quic_server.rs:handle_bridge_registration   → 响应加 recording_pubkey/key_id
        ↓
register.rs:connect_and_register (:156-159)  → 解析 ack，写入共享 Arc
        ↓
RegisterConfig.recording_pubkey (:68):
    Arc<tokio::sync::RwLock<Option<(String, String)>>>   // (base64 公钥, key_id)
        ↓
两个 handle_quic_stream 调用点都要传:
    register.rs:303-305   （注册模式）
    main.rs:172-174       （bridge 监听 / direct 模式）
        ↓
files.rs:34 handle_quic_stream 签名加 recording_pubkey (:47)
        ↓
files.rs:87-112  0x07 数据流分支 → interactive::handle_interactive_data(..., recording_pubkey) (:109)
        ↓
interactive.rs:317 handle_interactive_data 签名加 recording_pubkey (:328)
        ↓
interactive.rs:517-538  读 Arc → RecordingEncryptor::new(pk, kid, &filename)
        ↓
cast_recorder.rs:writer_task (:126)          → 使用 encryptor 加密
```

> **实现修正（v2.2）**：公钥经 **0x07 interactive 数据流分支**透传，**不经过 `proxy_protocol_aware`**（该函数只处理 0x01 JSON 协议帧）。共享 Arc 内类型是 `(String, String)`（base64 公钥 + key_id 字符串），非 `(PubKey, String)`。

- bridge **内存持有**，不落盘；断线重连重新获取（支持 server 轮换）。
- 公钥真实性由 `ca.crt` + TLS 保证。

### 3.3 密文格式（分块流式 AEAD）

> **v2 修正（原 CRITICAL-1）**：原 §3.3"整文件加密"与 §3.4"每事件加密"自相矛盾，且未定义 nonce。录制是**边录边写**（`cast_recorder.rs:123` writer_task 逐行增量写、64KB fsync），必须用**分块流式 AEAD**。

```
文件 = 明文头行 + 若干密文块

[明文头行]\n
  {"fmt":"clum-enc","v":1,"alg":"x25519-aes256gcm-v1",
   "key_id":"<16hex>","name":"<录制文件名 basename>","epk":"<base64 临时公钥>",
   "salt":"<base64 32B>","nonce_prefix":"<base64 8B>",
   "wrapped_dek":"<base64 48B>"}

[密文块]*   （每块独立 AEAD）
  块 = [4B LE payload_len][1B last_flag][密文+tag]
       payload_len = 密文字节数 + 16B tag（AEAD 输出总长）
  nonce = 12B = [4B 计数器(BE)] [8B nonce_prefix]   （计数器按块位置推断：0,1,2…）
  aad   = 头内 name 字段 || 4B 计数器(BE) || 1B last_flag
```

- **DEK**：每文件随机 32B；**salt**：每文件随机 32B；**nonce_prefix**：每文件随机 8B，**存入头**（解密方必需，缺失则无法构造 nonce）。
- **每块独立 tag** → 任意时刻崩溃，已写完整块均可解密；尾部截断可检测（长度前缀 + tag）。
- **分块粒度**：聚合到 ~64KB 或每个事件（小事件建议聚合，避免每行 38B 开销把文件撑大一倍）。
  - 注意：聚合会改变 fsync 粒度（原每 64KB fsync 一次 → 现按块 fsync），语义等价，实现需对齐。
- **nonce 唯一性**：同一 DEK 下计数器严格递增，**绝不重复**（GCM nonce 重用是灾难级）。
- **最后一块标记**：块头**显式 1B `last_flag`**（=1 即最后一块）；解密方读到即停止。计数器不存块里、按位置推断（第 n 块 counter=n），避免解密方无从得知 counter。
- **AAD**：`头内 name 字段 || 4B 计数器 || 1B last-flag`——**含块序号**，显式防重排/截断；不绑 host（见 §4.3）。
- **AAD 取头内 `name`（v2.2 实现修正）**：server push 时会把文件重命名为 `{agent}_{filename}`，故解密方**不能**用磁盘路径名做 AAD；`name` 字段记录加密时的原始 basename，是 AAD 的唯一来源（`crypto.rs` 中 `aad_name = filename.as_bytes()`，解密读 `header.name`）。

### 3.4 bridge 录制加密（代码落点）

**落点**：`cast_recorder.rs:123` `writer_task`

- 现状：收 `CastEvent` → `write_and_track` → `file.write_all`（:270-282）。
- 改动：
  1. 录制开始：写明文头行（含 key_id/epk/salt/wrapped_dek）
  2. 每个（聚合后的）事件：分块加密后写盘
  3. 结束/崩溃：写最后一块（带 last-chunk 标记）
- 公钥来源：经 §3.2 接线链传入的 `Arc<RwLock<Option<(String,String)>>>`（base64 公钥 + key_id）。

> **实现修正（v2.2）**：原 M7 设计为"有界缓冲等待注册"，**落地版本实际采用 fail-open 降级为明文**：
> 1. 公钥不可用（bridge 启动时 server 不可达 / direct 模式 / server 长期失联）→ 该录像**以明文写盘** + `tracing::warn!("recording public key unavailable, storing plaintext")`（`interactive.rs:531-536`）；
> 2. encryptor 构造失败 → 同样降级明文 + `tracing::error!`（`interactive.rs:523-528`）；
> 3. 另有独立的 channel 溢出保护：`cast_recorder` 在内部 channel 满时置 `gap_pending`，恢复后写 `[gap]\r\n`（`cast_recorder.rs:87-103`，与公钥无关，是既有机制）。
>
> **边界（重要）**：降级录像**不加密**——"全链路无明文"仅在公钥可用时成立；缓冲/审计告警尚未实现，列为后续项。触发场景：bridge 启动时 server 不可达、direct 模式、server 长期失联。

### 3.5 server 解密：格式嗅探 + 搜索（按个）

> **v2 修正（原 CRITICAL-2）**：存量明文不迁移（§8.3），但 server 必须**同时支持明文与密文**，否则 90 天窗口内所有历史录像静默变"搜不到"。

**统一解密入口**（新增 `decrypt_or_passthrough`）：
```
读首行：
  首行匹配 {"fmt":"clum-enc",...}  → 按 key_id 取私钥 → 分块解密 → 明文
  否则（legacy asciinema v2）        → 直接返回原字节（passthrough）
```

- **落点**：`search.rs:182` `scan_cast_file` 入口先走 `decrypt_or_passthrough`（`:195`）。
- **错误必须显式上报**：`search.rs:97-100` 当前解密失败只 `warn + Vec::new()`（表现为"无命中"）——改为在响应里加 `decrypt_errors` 计数与文件清单。
- 遍历逻辑不变（逐文件、可提前 break）；内存峰值 = 单文件明文大小。

### 3.6 server 解密：回放 + get_recording

> **v2 修正（原 M4）**：分组 key 被禁 HTTP `/recordings`（`http_server.rs:384` → 403），唯一通道是 MCP `get_recording`（`tools/mod.rs:469` → `read_recording_file`）。原清单漏了它，且 `read_recording_file` 用 `read_to_string` 读密文会直接报错。

| 入口 | 落点 | 改动 |
|------|------|------|
| HTTP `/recordings` | `http_server.rs:384` | 走 `decrypt_or_passthrough` 后返回；**保持 superadmin-only**（分组 key 仍 403） |
| MCP `get_recording` | `tools/mod.rs:469`→`:493 read_recording_file` | 走 `decrypt_or_passthrough`；把 `read_to_string` 改为 `read`（二进制安全） |
| 搜索 | `search.rs:182` | 见 §3.5 |

- cli replay 下载走 HTTP，**需 superadmin key**（文档明示；分组用户走 MCP `get_recording`）。

### 3.7 回放端 memfd（含平台降级）

> **v2 修正（原 M8）**：`memfd_create`/`O_TMPFILE` 均 **Linux-only**，而 README 明确 macOS 是 cli 构建平台；且 **curl 不能 `-o <fd>`**。

**落点**：`clum-cli/src/main.rs:314-355`（Replay 分支）+ `replay.rs:80`（`load_and_prepare`）

- 现状（`main.rs:323-334`）：`curl -fsSL -o $TMPDIR/clum-replay.cast` → 播放 → `TempGuard` 删除。
- 改动：
  1. **下载**：`curl -o -`（stdout）→ 父进程读管道写入目标；
  2. **Linux**：写 `memfd_create` 匿名文件 → 关闭 fd 自动回收；
  3. **macOS 降级**：`mkstemp` + 立即 `unlink`（**注意：macOS 上 unlink 文件仍为磁盘后备，"绝不落盘"仅 Linux 成立**——文档明示此边界）；
  4. `replay.rs` 的 `load_and_prepare` 签名从 `&Path` 改为收 `File`/fd。
- **seek/倍速/暂停零影响**（全事件载入内存，`replay.rs` 的 `rebuild_vt`/`calc_delay`）。

### 3.8 server 强化（层 3）

| 项 | 落点 | 改动 |
|----|------|------|
| 录像权限 0600 | `quic_server.rs`（push 落盘，~:420）、`recording_sync.rs:227`（pull 落盘） | `fs::write` 后 `set_permissions(0o600)` |
| ACK 前 fsync | `quic_server.rs`（push 落盘后） | `file.sync_all()` 后再发 `recording_ack` |
| **sha256 语义** | `cast_recorder.rs:270-282` + `recording_sync.rs:214` | **v2 修正（原 M3）**：`meta.sha256` 必须 = 对**最终落盘密文**的哈希（当前 `hasher.update` 哈希写入字节；改为哈希密文块）。否则 pull 同步按 sha 校验会**永久静默拒绝**每个新录像 |
| **sha 校验范围** | `register.rs:406 push_recording` | **push 通道不带 sha**（已核实 `recording_push` 消息无 sha 字段），sha 校验**只在 pull**（`recording_sync.rs:214`）。故 M3 仅影响 pull 兜底通道；push 主通道靠传输层完整性 |

---

## 4. 协议与数据结构

### 4.1 注册响应扩展

```json
// server → bridge（register_ack 成功时）
{ "type": "register_ack", "ok": true,
  "recording_pubkey": "<base64>", "recording_key_id": "<16hex>" }
```

### 4.2 密文文件头（明文）

```json
{
  "fmt": "clum-enc", "v": 1, "alg": "x25519-aes256gcm-v1",
  "key_id": "<16hex>", "name": "<录制文件名 basename>", "epk": "<base64 临时公钥>",
  "salt": "<base64 32B>", "nonce_prefix": "<base64 8B>",
  "wrapped_dek": "<base64 48B>"
}
```

### 4.3 分块帧

```
[4B LE payload_len][1B last_flag][密文+tag]
  payload_len = 密文 + 16B tag 的总长
  last_flag   = 1 表示最后一块
  counter     = 0,1,2,... 由块位置推断（不存块里）
nonce = [4B counter BE][8B nonce_prefix]   （12B；nonce_prefix 存文件头）
AAD   = 头内 name 字段 || 4B counter(BE) || 1B last-flag
        name = 加密时的原始 basename（如 user_sess__pane_epoch_rand.cast）
               —— 解密方读 header.name，不用磁盘路径名（server 会加 {agent}_ 前缀）
```

> **v2 修正（原 M9）+ v2.2 实现修正**：AAD 只绑**录制文件名**，不绑 host/date。原因：`recording_sync.rs:108-125` `merge_sync_hosts` 静态优先、host 名可能来自 hosts.yaml 或注册名，bridge 录制时无从得知 server 会用什么名；绑 host 会弄碎解密。文件名已含 user/session/pane/epoch/random，唯一性足够。**v2.2**：该文件名**不是磁盘路径名**，而是密文头内 `name` 字段——server push 会加 `{agent}_` 前缀重命名，故头内冗余的 `name` 正是 AAD 字符串本体（`crypto.rs` 用它构造 AAD，解密时读同一字段）。

### 4.4 X25519 信封流程（含 KDF 修正）

> **v2 修正（原 M10）**：HKDF 加 salt，并把 `epk||server_pub` 纳入输入（抗 unknown-key-share）。

```
加密（bridge）:
  (esk, epk) ← X25519 临时密钥对
  ss  ← X25519(esk, server_pubkey)                 // ECDH；server_pubkey = current 公钥
  salt ← 随机 32B（入头）
  nonce_prefix ← 随机 8B（入头）
  kek ← HKDF-SHA256(salt, ikm = ss || epk || server_pubkey, info="clum-recording-v1")
  dek ← 随机 32B
  wrapped_dek ← AES-GCM(kek, dek)
  逐块: 密文块 ← AES-GCM(dek, nonce=counter||nonce_prefix,
                        aad=header.name||counter||last_flag, 块明文)

解密（server）:
  server_pub ← 由 server_privkey[key_id] 推导（**必须是该 key_id 对应的公钥**；
               轮换后旧密文用旧公钥，否则 KDF 输入不一致 → 解密失败）
  ss  ← X25519(server_privkey[key_id], epk)
  kek ← HKDF-SHA256(salt, ss || epk || server_pub, "clum-recording-v1")
  dek ← AES-GCM-open(kek, wrapped_dek)
  逐块: 明文 ← AES-GCM-open(dek, 块, nonce=counter||nonce_prefix,
                            aad=header.name||counter||last_flag)
```

---

## 5. 代码改动清单（v2.2 实际落地）

> 实际改动：**19 文件修改 + 2 新增**（另含本设计文档）。行号为落地版本。

| 文件 | 改动 |
|------|------|
| `Cargo.lock` | 依赖锁更新 |
| `crates/clum-core/Cargo.toml` | 新增依赖 `x25519-dalek`、`aes-gcm`、`hkdf`、`base64`、`getrandom`、`sha2`（**改的是 clum-core 的 Cargo.toml，不是 workspace**） |
| `crates/clum-core/src/lib.rs` | 挂载 `pub mod crypto` |
| `crates/clum-core/src/crypto.rs` | **新增**：信封 + 分块流式 AEAD + `is_encrypted`/`decrypt_recording` + 11 测试 |
| `crates/clum-mcp/src/recording_keyring.rs` | **新增**：keyring（`current.key` + 扫描 `*.key`）+ `decrypt_or_passthrough` |
| `crates/clum-mcp/src/main.rs` | 加载 keyring（`resolve_recording_keys_dir` → `load_or_create`） |
| `crates/clum-mcp/src/server_config.rs` | `recording_keys_dir` 配置 + 解析 |
| `crates/clum-mcp/src/quic_server.rs` | 注册响应下发公钥；push 落盘 0600 + fsync；持有 keyring |
| `crates/clum-mcp/src/recording_sync.rs` | pull 落盘 0600 |
| `crates/clum-mcp/src/tools/search.rs` | `decrypt_or_passthrough` + `decrypt_errors` 上报 |
| `crates/clum-mcp/src/tools/mod.rs` | `get_recording`/`read_recording_file` 解密 + 二进制安全读取 |
| `crates/clum-mcp/src/tools/batch.rs` | 测试装配 keyring |
| `crates/clum-mcp/src/http_server.rs` | `/recordings` 解密后返回（保持 superadmin-only） |
| `crates/rmux-bridge/src/cast_recorder.rs` | writer_task 分块加密写盘；**sha256 改为哈希密文** |
| `crates/rmux-bridge/src/register.rs` | **接线**：ack 写入共享 Arc；`:303-305` 调用点传 pubkey |
| `crates/rmux-bridge/src/main.rs` | **接线**：`:172-174` 调用点传 pubkey；创建共享 Arc |
| `crates/rmux-bridge/src/files.rs` | **接线**：`:34` handler 签名加 pubkey → `0x07` 分支传给 `interactive`（**不经 proxy**） |
| `crates/rmux-bridge/src/interactive.rs` | **接线**：读 Arc → `RecordingEncryptor::new` → `CastRecorder::start` |
| `crates/clum-cli/src/main.rs` | `curl -o -` 管道 + `create_anon_file`（Linux memfd / macOS mkstemp+unlink） |
| `crates/clum-cli/src/replay.rs` | `load_and_prepare` 收 `File` |
| `clum-docs/recording-encryption-design.md` | 本设计文档（v2.2 与代码同步） |

---

## 6. 依赖变更（实际落地）

```toml
# crates/clum-core/Cargo.toml
x25519-dalek = { version = "2", features = ["static_secrets"] }
aes-gcm = "0.10"
hkdf = "0.12"
base64 = "0.22"
getrandom = "0.2"
sha2 = "0.10"
```

> 分块流式：**未用** `aead::streaming`，而是直接用 `Aes256Gcm` 逐块 `encrypt`（每块独立 nonce + 显式 `last_flag`），见 `crypto.rs:266-288`。

---

## 7. 测试计划（v2 补充）

| 测试 | 内容 |
|------|------|
| 单元：信封往返 | 加密→解密=原文；错密钥→失败 |
| 单元：分块往返 | 多块加密→解密=原文；**任意块截断→检测到** |
| 单元：nonce 唯一 | 同文件多块 nonce 不重复；**两次加密 (epk,salt,nonce) 必不同** |
| 单元：格式嗅探 | 明文 legacy 直读；密文解密；**混合目录两者都能搜** |
| 单元：sha256 语义 | meta.sha256 = 密文哈希；pull 校验必过 |
| 单元：AAD | 头内 name AAD 正确；改名后解密失败（预期） |
| 单元：公钥替换 | 错公钥加密→server 解密失败（不静默） |
| 单元：密文篡改 | 改一字节→GCM 校验失败 |
| 单元：按个解密 | 多文件搜索，逐个解密，提前终止 |
| 单元：memfd 回放 | 从 fd 加载 events；seek/倍速正常 |
| 集成：全链路 | bridge 加密→push→server 存密文→搜索命中→回放 |
| 集成：公钥下发 | 注册拿到公钥；断线重连重新获取 |
| 集成：降级 | 公钥缺失→**fail-open 明文落盘 + warn**（缓冲未实现，见 §3.4） |
| 集成：分组 key | `get_recording` 返回明文；HTTP `/recordings` 403 |
| 真机：dns-backup | 录制→bridge 本地密文→server 搜索/回放正常 |

---

## 8. 部署与迁移

1. **server 升级**：生成 keyring（首次启动自动），私钥离线备份。
2. **bridge 升级**：启动后注册时自动拿公钥（无需配置）。
3. **存量明文录像**：**不迁移**，等 90 天保留期自然淘汰——但 server **必须支持双格式嗅探**（§3.5），否则窗口内历史录像不可读。
4. **回滚**：格式嗅探使混合版本安全——旧 bridge 推明文、新 server 能读；新 bridge 推密文、旧 server 不认（需一起回滚 server 或接受新录像暂不可读，数据不丢）。
5. **密钥轮换**：追加新密钥，旧私钥保留至密文过期。

---

## 9. 风险与边界

| 项 | 说明 |
|----|------|
| **私钥丢失** | 对应密文永久不可读（已接受，靠离线备份降低概率） |
| **server 唯一副本** | 不备份（用户决定）；server 磁盘故障 = 录像丢失 |
| **公钥下发依赖注册** | 缺失时 **fail-open 降级明文** + `warn`（原 M7 缓冲未实现，见 §3.4）——这是"全链路无明文"的唯一缺口 |
| **搜索性能** | 逐文件解密，量大时慢 → 可后续并行化 |
| **回放延迟** | 每次重新下载（<1s） |
| **cli replay 需 superadmin key** | 分组 key 走 HTTP `/recordings` 会 403，replay 失效；分组用户需改用 MCP `get_recording`（升级前已如此，非本方案引入） |
| **macOS 回放落盘** | `mkstemp+unlink` 仍磁盘后备，"绝不落盘"仅 Linux 成立 |
| **root 沦陷** | 任何加密的共同边界 |
| **密码仍在录像内容里** | 只加密文件，不做内容脱敏 |

---

## 10. 实施顺序与工作量

| 阶段 | 内容 | 工作量 |
|------|------|:---:|
| 1 | server 强化：录像 0600 + fsync + **sha256 语义** | ~1d |
| 2 | `clum-core::crypto`：信封 + **分块流式 AEAD** + 测试 | ~2d |
| 3 | keyring + 公钥下发 + **5 文件接线链** | ~1.5d |
| 4 | bridge 录制加密（writer_task + 公钥缺失降级） | ~1.5d |
| 5 | server 解密：**格式嗅探** + 搜索 + get_recording + 回放 | ~2d |
| 6 | 回放端 memfd（含 macOS 降级） | ~0.5d |
| 7 | 集成测试 + 真机验证 | ~2d |

**合计约 10.5 人日**（v2 因分块格式 + 接线链 + 嗅探 + 降级，比 v1 的 8 人日增加）。阶段 1 可独立交付。

---

## 11. 实施与部署记录（2026-09-11）

**代码状态**
- 本地：`cargo test --workspace` 350 测试全绿；`cargo clippy --workspace --all-targets -- -D warnings` 零警告。
- 交叉编译：`just release-linux`。

**产物与部署**
| 组件 | 部署主机 | sha256 | 说明 |
|------|----------|--------|------|
| `clum-mcp` | aliyun-teleport | `9724858e…` | keyring `/root/.clum/recording-keys/current.key`（0600） |
| `rmux-bridge` | **11 台**（k8s-m1/m2/m3/n1/n2/n3/n4、rustfs、tf001、aliyun-teleport、dns-backup） | `f52efc12…` | 全部 active，key_id `8484385b6d7759ee` |

**端到端验证（dns-backup）**
- bridge 录像首行 = `{"fmt":"clum-enc",…,"name":"…"}`；明文标记 grep 命中 0。
- server `search_recordings` 命中解密后明文；`curl /recordings/...` 返回明文 asciinema。
- 旧格式（明文）录像走 passthrough，`decrypt_errors` 显式上报。

**已知偏离**
- 公钥缺失时 **fail-open 降级明文**（原 M7 的缓冲方案未实现，见 §3.4）。
- macOS 回放仍落磁盘后备（§3.7/§9）。

**部署教训（须写入部署 checklist）**
1. **上传 bridge 二进制后必须 `chmod +x`**——否则 systemd 启动失败（本次批量升级曾漏做，导致 10 台 bridge 服务全挂、MCP 通道中断）。
2. 批量升级应逐台校验 `systemctl is-active` + sha256，再放行下一批。
3. 远程操作优先用 `~/.ssh/config` 别名（避免 `root@IP` 直连失败）。

---

## 附：v1 → v2 修正对照

| 编号 | v1 问题 | v2 修正 |
|------|---------|---------|
| C1 | §3.3 整文件 vs §3.4 每事件矛盾、无 nonce | §3.3 定义分块流式 AEAD（长度前缀 + counter nonce + 独立 tag） |
| C2 | 存量明文静默变"搜不到" | §3.5 格式嗅探 + `decrypt_errors` 显式上报 |
| M3 | sha256 语义未定义 | §3.8 明确 = 密文哈希 |
| M4 | 分组 key 拿不到明文 | §3.6 补 `get_recording`/`read_recording_file` 解密 |
| M5 | 公钥接线漏 files/interactive | §3.2 补全 5 文件 / 2 调用点接线链 |
| M6 | 密钥轮换只做一半 | §3.1 改版本化 keyring + key_id |
| M7 | fail-closed 丢事件 | §3.4 改有界缓冲 + error 告警 |
| M8 | memfd/curl 不成立 | §3.7 curl `-o -` + macOS 降级 + 边界明示 |
| M9 | AAD 绑定不精确 | §4.3 AAD = 文件名 |
| M10 | KDF 无 salt | §4.4 加 salt + transcript 绑定 |

## 附：v2 → v2.1 修正对照（本轮评审）

| 编号 | v2 问题 | v2.1 修正 |
|------|---------|-----------|
| N1 | nonce 前缀未存文件头 → 密文不可解 | §3.3/§4.2 头加 `nonce_prefix` 字段 |
| N2 | 块长度语义未定（含不含 tag） | §3.3/§4.3 明确 payload_len = 密文字节数 + 16B tag（AEAD 输出总长，**含 tag**） |
| N3 | last-chunk 标记两方案并列 | §3.3 选定**块头 1B last_flag**（实现时修正：计数器高位方案解密方无从得知 counter，改为显式标志） |
| N4 | AAD 未含块序号 | §3.3/§4.3/§4.4 AAD 改为 `文件名‖counter‖last-flag` |
| N5 | KDF 的 server_pub 来源未指明 | §4.4 明确 = 该 `key_id` 对应公钥 |
| N6 | replay 需 superadmin 未入风险表 | §9 补风险行 |
| N7 | 聚合改变 fsync 语义未说明 | §3.3 补说明 |
| N8 | push 不校验 sha 未点明 | §3.8 补行说明校验范围 |
