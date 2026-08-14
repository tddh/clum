# Changelog

## [0.14.1] — 2026-08-14

### Fixed
- **录制 push 通道推送半成品**（`rmux-bridge/register.rs`）：`scan_cast_files` 仅按 `*.cast` 后缀扫描，不区分录制是否完成——录制中的文件（无 `.meta`，`.meta` 仅 finalize 时生成）会在 30s push 周期内被当作已完成推送给 server，推过即记录不再重推，完整版只能依赖 pull 兜底。实测 server 端 97 个录制中 49 个缺 `exit` 事件（半成品）。修复：只推送「有 `.meta` 且 `synced=false`」的已完成文件，与 `list_unsynced` 语义对齐。
- **录制 pull 兜底通道空转**（`clum-mcp/recording_sync.rs`）：`sync_all_hosts` 只遍历 `router.list()`（hosts.yaml 静态主机），纯 enrolled 部署（`hosts.yaml: []`，桥全部动态注册）时遍历空列表，pull 通道从未拉取，半成品对应的完整版永远无法补拉。修复：`merge_sync_hosts` 合并 enrolled bridges（`registry.list()`）与静态主机并集，静态配置优先、按主机名去重；enrolled 合成主机无 `bridge_addr/token`，走已注册 QUIC 连接。
- **录制下载协议错位**（`clum-mcp/recording_sync.rs`）：`download_recording` 按 `[status][8B size][32B sha256][data]` 解析，但 bridge 端 `download_file_quic` 响应**不含** 32B sha256——每个文件前 32 字节被当作 sha 吞掉，`read_exact(file_size)` 读不满报 `stream finished early`，下载全部失败。修复：去掉 sha 读取，文件完整性由调用方按 `list_unsynced` 返回的 `expected_sha` 校验（`sha256` 不匹配仍会跳过）。
- **recording sync 周期错过时背靠背补发**（`clum-mcp/recording_sync.rs`）：`run_sync_loop` 的 `interval` 未设置 `MissedTickBehavior`（默认 `Burst`）——单轮 `sync_all_hosts` 执行超过周期（300s）时 tick 立即连续补发，大量 unsynced 下载会把多轮 sync 挤在一起背靠背运行。修复：改为 `Skip`（错过即跳过，等下一个周期），与 `interactive.rs` / `tui` 的既有处理一致。

## [0.14.0] — 2026-08-14

### Added
- **`batch_send_keys` MCP 工具**：多主机并发发送按键（投递确认语义，fire-and-forget），发完即返回、不等待命令执行结果。补齐「跨主机 + fire-and-forget」空缺（`batch_exec` 同步等待、`broadcast_keys` 仅单主机多 pane）。结果用 `capture_pane` / `wait_for_text` 逐台查。
- **`clum-cli term` 自动创建会话**：`term <host> --session <name>` 在会话缺失时自动创建 detached 会话（原报错退出），中央 server 与直连模式均支持。
- **移除文件传输 2GB 硬上限**：clum-mcp 的 `file_upload` / `file_download` 不再限制单文件大小（删除 `MAX_FILE_SIZE` 常量与检查）。

### Changed
- **错误码系统重构（治本）**：
  - 分类逻辑下沉到 `clum_core::error_code` 共享（19 个稳定错误码），MCP 与 bridge 两端分类永远一致。
  - bridge 响应出口（`send_response`）自动注入 `error_code`；MCP 侧 `enrich_error` 优先采用桥侧码并补齐 `recovery_hint` / `retryable`——所有错误信封三件套保证完整，双向协议兼容（旧 MCP 忽略新字段，旧 bridge 由消息分类兜底）。
  - 新增错误码：`CONNECT_TIMEOUT`（连接超时，可重试）、`CLI_FAILED`（bridge rmux CLI 回退失败）、`PROTOCOL_ERROR`（帧协议错误）；错误码只增不改。
  - 修复 `invalid pane_id` / `invalid source_pane_id` / `invalid target_pane_id` 等 30+ 处高频错误从 `UNKNOWN` 归入 `PANE_NOT_FOUND`。
  - 修复 `host 'x' not found in enrolled bridges` 归入 `HOST_NOT_FOUND`、`path contains null byte` / `directory too deep` 归入 `PATH_TRAVERSAL` 等漏匹配。
  - 修复 `batch_*` / `deploy_bridge` 空 hosts 返回 `ok:true + error` 的语义矛盾，改为 `ok:false`（`INVALID_PARAMS`）。
  - 修复 `resolve_pane_id` 假透传（`[UNKNOWN]` 前缀包装实为重分类），直接透传原始消息。
  - 参数值域错误采用锚定匹配（`must be 0-65535` 等），避免误伤状态类消息。
  - 文档：`docs/TOOLS.md` 错误码表同步（+3 码、AUTH_FAILED/TIMEOUT 描述修正、deploy status 说明）；新增 `docs/error-code-design.md` 方案文档。
- **bridge 会话默认工作目录**：新建 session 的默认 cwd 从 `/` 改为用户家目录（HOME），覆盖 MCP `session_create` 与 CLI `term` 全部入口。

### Fixed
- **bridge SIGTERM 优雅关闭**：收到 SIGTERM 主动向 server 发送 CONNECTION_CLOSE 帧并 flush，server 立即 unregister——`systemctl restart rmux-bridge` 不再因旧连接等待 120s idle timeout 而出现 duplicate 注册窗口。
- **bridge enrolled 模式不再启动本地 QUIC listener**：仅直连模式（未配置 server_addr）监听 9778，消除多余端口占用与攻击面。
- **录制链路静默失败**：`cast_recorder` 的 fsync/flush、`register` 的 save_pushed、`interactive` 的录制目录权限设置，失败从 `let _ =` 静默忽略改为 `tracing::warn` 记录，避免录制数据丢失、重启重复推送、目录权限过宽。
- **多处 panic 风险消除**：`build_transport_config` 的 idle_timeout VarInt 溢出 expect 改返回 `Result`；MCP `router` 锁中毒 expect 改为 `into_inner()` 恢复；`getrandom` CSPRNG 失败从 expect 改为错误传播（`generate_bridge_token`/`generate_key` 返回 `Result`）；bridge 注册循环 `reg_handle.await` 不再忽略；CLI `transfer` 的 `unreachable!` 改为优雅降级。
- **文件传输错误信封缺失**：bridge `handle_upload_quic` / `handle_download_quic` 在 `sanitize_path` 拒绝（路径穿越等）时直接 bail 不发送错误响应，MCP 端读到 EOF 归为 `UNKNOWN`。改为发送错误信封（0x02 + 消息），MCP 端 `upload_single`/`upload_dir` 解析 0x02 分支，`PATH_TRAVERSAL` 等错误码正确传播。
- **下载 local_path 未校验路径穿越**：MCP `download_file`/`upload_file` 的本地路径（server 侧）含 `..` 未拒绝（可写入任意路径）。新增 `sanitize_local_path`（拒绝 `..`/null byte，规则与 bridge `sanitize_path` 一致）并在入口校验。

### Refactored
- **帧协议收敛**：`read_frame`/`write_frame`（LE32 长度前缀 JSON 帧）从 clum-mcp 与 rmux-bridge 两处逐字节相同的实现提取到 `clum_core::quic`，wire 协议不变。
- **`SessionTracker` 封装**：`session_state`/`session_counts` 的类型重复在 main（直连）与 register（注册）两处收敛为单一 struct。
- **常量统一**：1MB 传输缓冲（`COPY_BUF_SIZE`）4 处重复定义收敛到 `clum-core`；30s/60s/10min 超时默认值 13 处 magic number 改为 `DEFAULT_WAIT/COLLECT/EXEC_TIMEOUT_MS` 命名常量。

## [0.13.0] — 2026-08-13

### rmux 0.10 升级
- **rmux-sdk 0.9.1 → 0.10.0**：wire protocol 5→8（硬切断），daemon 与 bridge 必须同步升级；升级会重启 daemon，**所有 rmux session 丢失**。
- **stream_pane 迁移 recover_output**：lag/resize/清屏/parser 过期后自动 in-band rebase 修复，不再丢输出。
- **collect_until_exit**：0.10 内部输出采集机制增强（代码零改动）。
- **部署脚本固定 RMUX_VERSION=0.10.0** + helper 完整性检测（`rmux list-commands`）+ 版本验证，修复"只拷 bin 二进制缺 libexec helper"导致的 term 连上即退问题。

## [0.12.0] — 2026-08-13

### Added
- **`clum-core::backoff`：统一 Full Jitter 退避原语**（`FullJitterBackoff`，基于 `fastrand`，零额外传递依赖）。公式 `sleep = random(0, min(cap, base * 2^attempt))`，含溢出防护（checked_shl + saturating）、成功后 `reset()` 归零、`with_seed` 确定性测试构造。消除多实例同步重连的惊群效应。

### Changed
- **全项目退避统一为 Full Jitter**（替换 5 处手写指数退避，参数逐处对齐）：

| 调用点 | base → cap |
|--------|-----------|
| bridge 注册循环（`register.rs`） | 500ms → 30s |
| MCP QUIC 连接重试（`transport.rs` `with_retry`） | 500ms → 8s（原无封顶，新增） |
| CLI `forward` 重连（`forward.rs`） | 1s → 30s |
| CLI `term` 重连（`tui/mod.rs`） | 1s → 30s |
| `exec` 断连续等（`tools/exec.rs`） | 500ms → 5s |

  所有调用点保留原有语义：成功重置退避、`forward` 的 `--give-up-after` 总超时裁剪、`exec` 的 deadline 预算裁剪、`with_retry` 的 `max_retries` 计数。
- **bridge 交互会话按 client_id 隔离**（`interactive.rs`/`register.rs`）：新增 `SessionCounter` 跨连接共享活跃连接计数，连接断开时若会话仍有其它活跃 client 则跳过 layout restore，避免 even-vertical 重排误伤其它连接的显示；recording 文件归属按 client 隔离。

### Fixed
- **CLI term（Windows）**：进程退出检测与 pane 恢复相关修复。

### Docs
- CHANGELOG：记录 0.12.0 Full Jitter 退避统一改造与交互会话隔离。

## [0.11.0] — 2026-08-11

### Added
- **文件传输带宽限速**：`clum-core::rate_limiter` 新增 token-bucket 限速器（`RateLimiter`/`BandwidthLimiter`，AtomicU64 实现，零依赖），以 1MB chunk 粒度注入所有 copy/recv 循环：
  - **MCP**：`file_upload`/`file_download` 新增 `bandwidth_limit_mbps` 参数（0=不限速，默认取服务端配置）。
  - **CLI**：`push`/`pull` 新增 `-B/--bw-limit <Mbps>`（默认 0=不限速）。
  - **服务端配置**：`server-config.yaml` 新增 `file_transfer` 节——`upload/download_bandwidth_mbps`（**默认 70Mbps**，防止公网带宽打满导致断连）、`global_*_bandwidth_mbps`（全局聚合限速，0=不限）、`max_upload_concurrency`（默认 16）。
  - 优先级：CLI/MCP 显式传值 > 服务端配置 > 不限速；`None/0` 向后兼容。
- **`search_recordings` MCP 工具**：全文搜索 asciinema 录制内容，支持 substring/regex 匹配、ANSI escape 剥离、上下文行输出，按 host/日期范围/session/事件类型（输入/输出）过滤；逐行流式扫描，解析失败行跳过不报错。用于回答"这条命令什么时候跑的 / 这个错误在哪出现过"，与 `audit_query`（谁做了什么）互补。

### Changed
- **CLI**：`upload`/`download` 子命令重命名为 `push`/`pull`（旧名保留为 alias，现有脚本不受影响），transfer 用户可见字符串、README、TOOLS.md、SKILL.md 同步。

### Removed
- **移除全部 yunying 遗留兼容层**（0.10.0 过渡期正式结束）：
  - `YUNYING_*` 环境变量 fallback（bridge/cli/clum-core 三处）。
  - `/etc/yunying/token` 文件路径 fallback。
  - QUIC ALPN 仅保留 `b"clum"`，移除 `b"yunying"`。
  - 删除死代码 `inject_env_fallback`。
  - ⚠️ **破坏性变更**：升级前必须先升级所有 bridge 到 0.10.x，否则旧 bridge 直连新 server 将 `QUIC handshake failed`。

### Fixed
- `ai_panel.rs`：`unwrap()` 改为防御式 `if-let`，避免异常路径 panic。
- **CLI push 目录路径报错**：`push` 的远端目标以 `/` 结尾（如 `/tmp/`）时，旧实现把目录路径原样传给 bridge，bridge 无法将临时文件 rename 到目录且不回传 status，CLI 误报 `stream finished early (0 bytes read)`。新增 `resolve_remote_file`——目标以 `/` 结尾时自动拼接本地文件名（对齐 scp 语义），并在 push 摘要中显示实际落盘路径。

### Quality
- 新增 GitHub Actions CI pipeline（`.github/workflows/ci.yml`）：`cargo check` / `cargo fmt --check` / `cargo clippy -D warnings` / `cargo test` 四 job。
- 测试补全 **+102 个**：api_keys +16、common +11、bridge_store +9、error +6、types +7（累计两轮共 +102）。
- `.opencode/skills/clum-mcp/SKILL.md` 增强：新增运维模式、执行验证、不二过、分阶段汇报、经验沉淀章节。

### Docs
- README/README.zh：`push`/`pull` 重命名与限速参数说明。
- docs/TOOLS.md：`search_recordings` 工具定义与 `bandwidth_limit_mbps` 参数同步。
- docs/DEPLOY.md：安装脚本 socket 检测与部署说明对齐。

## [0.10.2] — 2026-08-06

### Added
- **term**：断线检测（`conn.closed()`）与自动重连（1s→30s 指数退避），重连后回放 attach scrollback 恢复屏幕；AI 面板状态跨重连保留。
- **term**：读取 ctrl 流 `0x83 process_exited`，远端卸载（Ctrl+B D）或 pane 进程退出时干净退出并提示 `term: detached (exit code N)`，不再卡死。
- **clum-core::quic**：新增共享 QUIC 传输层（`build_transport_config`/`client_endpoint`/`connect_bridge`/`authenticate_bridge`），clum-mcp、clum-cli、rmux-bridge 统一复用，消除 6 处重复建连代码与参数漂移（窗口/BBR/keepalive 归一）。
- **CLI upload/download**：进度条（stderr 单行 10Hz 刷新，非终端静默）。单文件显示 `↑/↓ 文件名 [bar] % 已传/总量 速度`；目录上传为聚合进度条 + `(N/M files)` 完成计数；结束摘要升级为可读单位 + 耗时 + 速度。
- **目录下载并行化**：bridge 新增 `0x08` 清单流（只返回文件列表与大小），CLI 先拉清单再 16 并发逐文件下载，聚合进度条与上传样式一致（总量预知）；旧版 bridge 不支持清单流时自动回退串行下载。

### Changed
- **MCP 工具**：`session_name` 全部工具缺省默认 `"clum"`（原为必填报错），schema 移出 required，TOOLS.md 同步。
- **错误码**：`TUNNEL_DENIED` → `FORWARD_DENIED`（与 docs/TOOLS.md 及 `FORWARD_NOT_FOUND` 对齐）。
- CLI 直连 bridge 新增 10s 握手超时（原无限等待）。
- `forward_create` 建连不再挂保活 auth 流任务（握手完成即关闭）。
- `http_server.rs`：结构体 `YunyingServer` → `ClumServer`（0.10.0 改名遗漏清理）。
- `deploy/install.sh`：新增 rmux socket 自动检测（与 `deploy-bridge.sh` 逻辑一致，未检测到时回退 `/root/.rmux/rmux-0/default`），不再硬编码。
- `deploy/deploy-bridge.sh`：bridge.env 补写 `RECORDING_DIR=/opt/clum/recordings`，与 `install.sh` 对齐（此前缺失导致录制落入二进制同目录的默认路径）。
- download token TTL 接入 `server-config.yaml` 的 `token_ttl_hours`（默认 24h），不再硬编码 1 小时。
- justfile：移除 `update-all-bridges`（硬编码 IP 清单，改用 `deploy_bridge` MCP 工具）与 `deploy-mcp`（直接用 `bash deploy/deploy-mcp.sh`）。
- CLI 日志默认过滤 `quinn_udp=error`（UDP 背压 WARN 属正常重传，不再打断进度条），`RUST_LOG` 仍可覆盖。

### Fixed
- **bridge_store**：`list`/`token_map`/`get_all_host_meta` 去除 SQLite unwrap，异常时记日志并降级为空集（此前会导致 MCP server panic 或 token 刷新任务静默死亡）。
- **bridge 文件下载**：目录遍历改用 `symlink_metadata` 跳过符号链接，防止链接指向范围外的文件被带出。
- **deploy/install.sh**：`bridge.env` 创建后 `chmod 600`（此前全新安装为默认 umask 权限，token 可被本地任意用户读取）。
- README/README.zh：`connect` → `term` 重命名遗漏。
- docs/TOOLS.md：`list_recordings` 示例补齐 0.10.1 新增字段。
- `.qoder/skills/clum-mcp/SKILL.md`：`TUNNEL_DENIED`/`TUNNEL_NOT_FOUND` 更新为 `FORWARD_*`。

### Removed
- `scripts/migrate-to-yunying.sh`：agent-ops → yunying 时代的迁移脚本，已被 `scripts/migrate-to-clum.sh` 取代。

### Docs
- AGENTS.md：新增开发/测试隔离规则（测试必须用独立会话，严禁破坏默认 `clum` 会话）。
- 全面对齐文档与 0.10.0 实现：TOOLS.md（deploy_bridge `restart_sent` 状态、`FORBIDDEN` 错误码、`resolved_pane_id`/`auto_resolved`、tunnel `group` 字段）、terminal-state-design.md（§3.3 伪代码同步、新增 §4.6 exec 安全门禁）、connect-design.md（§6.2.2 过时标注）、DEPLOY.md（bridge.env 变量清单、socket 检测表述）、SECURITY.md（TLS 表述、HTTP 端点防护）、SKILL.md（`FORBIDDEN`/`UNKNOWN` 错误码）。

## [0.10.1] - 2026-08-06

### Changed
- **CLI**：`connect` 子命令重命名为 `term`，`--readonly` 改为 `--watch`。
- **replay**：基于 `avt` 虚拟终端的交互式回放，支持 seek（←→ ±30s）、调速（↑↓）、暂停（Space）。
- **replay**：默认 `idle_limit=0.5s`，自动跳过空闲段；状态栏显示 `HH:MM:SS` 格式时间和录制真实时间戳。
- **replay**：远程录制临时文件退出时自动清理。
- **term**：Ctrl+C 在普通模式下转发到远端（中断命令），`--watch` 模式下断开连接。
- **list_recordings**：增强返回字段，新增 `user`、`session`、`pane`、`started_at`、`duration_secs`。

### Fixed
- **connect.rs** → `term.rs`：文件名和模块引用重命名。
- `cargo fmt --all` + clippy 清理。
- `--watch` 逻辑反转修复。
- 测试断言 `TUNNEL_NOT_FOUND` → `FORWARD_NOT_FOUND`。

### Removed
- `docs/connect-design.md`（功能已内化到各文件）。

## [0.10.0] — 2026-08-04

**项目改名：yunying → clum**（沿革：agent-ops → yunying → clum）。这是一次破坏性改名，请仔细阅读以下迁移说明。

### Breaking
- **crate 与二进制**：`yunying-core/mcp/cli` → `clum-core/mcp/cli`；二进制 `yunying-mcp`/`yunying-cli` → `clum-mcp`/`clum-cli`。release 产物更名为 `clum-{macos-arm64,linux-x86_64,windows-x86_64}`。
- **MCP 客户端配置**：server key 建议从 `yunying` 改为 `clum`（工具前缀随之变为 `mcp__clum__*`），工具 `yunying_usage_rules` → `clum_usage_rules`。
- **环境变量**：`YUNYING_SERVER_ADDR/API_KEY/CA_CERT` → `CLUM_SERVER_ADDR/API_KEY/CA_CERT`。过渡期内旧变量仍作为 fallback 生效（启动时打印 deprecation 警告）。
- **默认会话名**：`yunying` → `clum`。远端已有的 `yunying` 会话保留不动。
- **远端路径**：`/etc/yunying` → `/etc/clum`、`/root/.yunying` → `/root/.clum`、`/opt/yunying` → `/opt/clum`、`yunying-mcp.service` → `clum-mcp.service`。部署脚本内置幂等迁移前奏（自动备份 + 原子替换）。
- **QUIC ALPN**：新标识 `clum`。**顺序铁律**：必须先升级中央 server（0.10 起双 ALPN 兼容 `clum`+`yunying`），再滚动升级 bridge/cli；新组件直连旧 server 会 `QUIC handshake failed`。

### Compatibility (transitional)
- Server 双 ALPN：接受 `clum` 与旧 `yunying`，未升级的 bridge 不断连。计划在所有 bridge 升级完成后的下一版本移除 `b"yunying"`。
- `YUNYING_*` 环境变量 fallback 与旧 token 路径 `/etc/yunying/token` 读取，计划随 ALPN 旧值一并移除。
- 已部署的 CA 证书（CN=yunying-ca）继续有效，不强制重签；新签发 CA 使用 CN=clum-ca。
- 历史审计日志（audit.db）原样迁移保留。

### Added
- `scripts/migrate-to-clum.sh`：本机 `~/.yunying` → `~/.clum` 数据目录迁移脚本。
- `clum_core::inject_env_fallback`：CLUM_*/YUNYING_* 环境变量兼容辅助函数。

## [0.9.2] — 2026-08-03

### Security
- **Bridge / Download token 权限收敛**：此前 bridge token 与 download token 验证通过后可访问全部非 `/mcp` 路由，包括 `/admin/download-token`（签发下载令牌）和 `/recordings/*`（**所有主机的终端会话录制**）。现收敛为仅 `/releases/` 前缀（部署产物），其余路径返回 403 并记录告警日志。
- **部署产物路径统一**：`/install.sh`、`/ca.crt` 移入 `/releases/` 下（根路径独立路由移除），一键部署 URL 变为 `https://SERVER:9788/releases/install.sh`。注意：已部署 bridge 不受影响（运行时走 QUIC）；重新执行一键部署时需使用新 URL，服务器 static_dir 中需将 `install.sh`、`ca.crt` 移入 `releases/` 子目录。
- **install.sh 去除 eval 注入面**：`eval curl ${AUTH}` 改为直接 `curl -H "Authorization: Bearer ..."` 调用。

### Added
- HTTP 认证中间件测试：白名单判定 + 各类凭证（API key / bridge token / download token / 无效凭证）的 403/401 行为，共 5 个用例。
- **clum-cli 文件传输增强**：`push`/`pull` 支持目录传输（目录上传逐文件并发 + `--exclude` glob 过滤；目录下载走 bridge 0x04 协议，相对路径做穿越校验），并改为 1MB 分块流式传输（不再整文件读入内存），输出含 SHA-256 校验。
- **yunying-cli tunnel 自动重连**：断网后本地端口保持监听，QUIC 连接死亡即时检测（idle timeout 3600s→60s）+ 指数退避重连（1s→30s）；`--give-up-after` 控制断网多久后退出（默认 2h，`0` = 永不退出）。

### Fixed
- **Bridge 轮换 token 内存生效**：此前 token 轮换只把新 token 落盘 `/etc/yunying/token`，运行中的 bridge 仍用启动时加载的旧 token 注册，Server 重启后触发 `unknown token` 失联。现 token 经 `Arc<RwLock>` 共享，轮换推送后立即在内存生效（不触发重注册），下次注册尝试即用新 token。
- **deploy-mcp.sh 不再覆盖远端 hosts.yaml**：此前会用本地占位文件无条件覆盖服务器上的权威配置；现仅在远端文件不存在时创建空文件。

## [0.9.1] — 2026-08-03

### Added
- **Group 隔离（RBAC）**：API Key 绑定 group（`agent add --group <g>`），Server 侧统一鉴权。组内 Key 只能访问本组主机，`host_list`/`host_filter`/`audit_query`/`list_recordings` 自动过滤，隧道按组隔离，`reload_config`/`host_set_meta` 仅超管可用。无 group 的 Key 为超管（向后兼容）。CLI 数据面（QUIC agent_connect）同步强制执行。
- **Bridge 超时自动重连**：`ProtocolProxy` 包裹 `RwLock`，rmux SDK 连接超时后自动重连（generation 防并发重复重连）。
- **QUIC 传输调优**：Bridge 客户端增加 idle timeout 120s、keep-alive 15s、BBR 拥塞控制、16MB 收发窗口；Server 端同步 idle timeout。
- **Recording sync 连接复用**：录制同步优先使用 BridgeRegistry 已注册连接，避免每次同步新建 QUIC 连接（未注册主机回退直连）。

### Fixed
- **Schema 审计修正**：修复 4 个 bug（`split_pane_with` 缺少 direction required、`respawn_pane` env 类型错误等）、7 处描述不准确；Skill 精简；路由改为 registry-first。
- **deploy_bridge 验证可靠性**：移除重启后的重连 + `systemctl is-active` 验证循环（与 bridge 重连过程竞态），改为直接返回 `restart_sent`；`host_list` 的 `online` 从硬编码 `true` 改为实时连接状态（`close_reason().is_none()`）。

### Changed
- **`agent add` 必须显式指定权限**：`--group <g>`（受限 Key）或 `--admin`（超管 Key）二选一，不指定直接报错，防止误建超管 Key。
- **部署路径调整**：`deploy-bridge.sh` 和 `install.sh` 统一将二进制部署到 `/usr/local/bin/`，数据目录（recordings、bridge 审计库）回退为 `/opt/agent-ops/`，并清理 `/opt/yunying/` 残留（0.8.0 曾迁移到 `/opt/yunying/`，实际未全面落地）。注：旧脚本 `install-bridge.sh` 未同步此变更，仍使用 `/opt/yunying/`。

## [0.9.0] — 2026-07-31

### Added — Central Server (Hub Mode)
- **中央 MCP Server**：双栈监听 TCP :9788（HTTP/2，rmcp StreamableHttpService）+ UDP :9788（QUIC，ALPN "yunying"）。AI 客户端只需配置一个 URL + API Key 即可连接。
- **Bridge 反向注册**：Bridge 主动连接 Server 并注册（token 认证），心跳保活（15s），断连指数退避重连（500ms→30s）。
- **连接注册表**：Server 内存注册表（`BridgeRegistry`），工具调用优先走 Hub 路由，未注册主机回退直连。
- **API Key 认证**：`yk_{name}_{32hex}` 格式，SQLite 存储，SHA-256 哈希。HTTP（Bearer）+ QUIC（agent_connect）双通道认证。
- **管理命令**：`yunying-mcp agent add/list/rotate/revoke`、`yunying-mcp bridge add/list/remove/join`。
- **server-config.yaml**：YAML 配置文件支持（listen、certs、bridges、token TTL），CLI 参数覆盖。
- **SSE 进度通知**：通过 rmcp `Peer.send_notification(ProgressNotification)` 实现 HTTP 模式实时进度推送。
- **audit_query MCP 工具**：查询 Server 侧集中审计日志（谁、何时、哪台机器、什么操作）。
- **审计身份注入**：API Key 验证后提取 agent name，审计日志记录操作者身份。
- **host_list 在线状态**：返回 `online: true`（Hub 连接）或 `null`（直连/未知）。

### Added — CLI 数据平面
- **`yunying-cli upload/download`**：文件传输通过 Hub 中继（0x02/0x03 协议）。
- **`yunying-cli tunnel`**：本地端口转发，per-connection QUIC stream 中继（0x05 协议）。
- **`yunying-cli connect`**：PTY 透传通过 Server 中继（agent_connect + 透明字节转发）。
- **`yunying-cli list`**：会话列表（通过 Hub 或直连）。
- **`yunying-cli replay`**：远程录制回放（从 Server HTTP 拉取 .cast 文件）。
- **全局选项**：`--server-addr`（Hub 模式）、`--api-key`（认证）、环境变量 `YUNYING_SERVER_ADDR`/`YUNYING_API_KEY`。

### Added — 录制与部署
- **录制推送**：Bridge 定期扫描 .cast 文件并推送到 Server（替代 Server 拉取），文件名带 agent 标识。
- **install.sh 一键部署**：`curl | sh` 自动下载二进制 + CA 证书 + 配置 systemd + 启动。
- **静态文件服务**：Server HTTP 端口提供 `/install.sh`、`/ca.crt`、`/releases/*`、`/recordings/*`。
- **Token 自动轮换**：24h TTL，Server 通过 QUIC 控制流推送新 token，Bridge 持久化到 `/etc/yunying/token`。

### Changed
- **MCP 协议升级**：rmcp v3.0.0（2026-07-28 spec），Streamable HTTP 传输。
- **架构文档更新**：SKILL.md、MCP instructions 同步 Hub 架构 + CLI 命令。

## [0.8.0] — 2026-07-29

### Changed
- **项目更名 agent-ops → yunying**：所有 crate 重命名（`agent-ops-cli` → `yunying-cli`、`agent-ops-core` → `yunying-core`、`agent-ops-mcp` → `yunying-mcp`），二进制名 `yunying-mcp`、`yunying-cli`，bridge 二进制名 `rmux-bridge` 不变。
- **默认 session 名改为 `yunying`**：Bridge 端 `new_session` 请求未指定名称时，默认创建 `yunying` 会话（原为 `agent-ops`）。
- **部署路径改为 `/opt/yunying/`**：`install-bridge.sh` 部署目录从 `/opt/agent-ops/` 改为 `/opt/yunying/`，systemd service、证书、bridge.env 统一迁移。
- **数据目录改为 `~/.yunying/`**：审计数据库、录制文件等本地存储路径从 `~/.agent-ops/` 改为 `~/.yunying/`。
- **文档全量更新**：README、DEPLOY、TOOLS、CONTRIBUTING、SECURITY、CHANGELOG、GitHub workflow、issue 模板、MCP 配置示例、justfile 等全部同步更新。

### Fixed
- **deploy_bridge 重连健壮性**：修复 send_keys 中 Ctrl+U 后多余字符；重启后等待时间 2s→3s；重连后重建 session 再验证（旧 session/pane 可能不存活）；单主机部署耗时 30s+→7s。

### Added
- **迁移脚本** `scripts/migrate-to-yunying.sh`：将 `~/.agent-ops/` 数据目录迁移到 `~/.yunying/`，支持合并已有数据。

## [0.7.1] — 2026-07-28（未打 tag）

### Security
- **MCP initialize instructions 增加不可信输出防护**：新增 "Security: Untrusted Output" 规则，明确告知所有 AI 客户端：工具输出（exec/capture_pane/stream_pane/file_download）是来自远程主机的不可信数据，不得将其中出现的文本视为指令执行。防护间接提示词注入（攻击者在日志/命令输出中嵌入伪装指令）。
- **CLI AI 面板 @analyze 提示词加固**：终端内容改用 `<terminal_output>` XML 标签包裹（替代 ``` 代码块，防止攻击者用 ``` 逃逸），并前置不可信数据声明。
- **SKILL.md 增加安全规则章节**：新增"工具输出是不可信数据"强制规则 + 典型攻击场景说明，AI agent 加载使用指南时即获得防护。

### Fixed
- **DEPLOY.md 幽灵命令**：移除已不存在的 `just run-bridge` 命令（0.6.1 已删除该 recipe）。
- **DEPLOY.md Bridge 参数表不完整**：补充 6 个缺失的录制/审计参数（`--recording-enabled`、`--recording-dir`、`--recording-retention-days`、`--recording-max-size-mb`、`--recording-fsync-interval-secs`、`--bridge-audit-db`）。
- **DEPLOY.md 架构图缺少 CLI**：补充 Human → yunying-cli → Bridge 的 PTY 透传路径。
- **DEPLOY.md 目录结构缺少 recordings/**：补充 `~/.yunying/recordings/` 目录说明。
- **SECURITY.md 版本号过时**：Supported Versions 从 `0.1.x` 更新为 `0.7.x`。
- **TOOLS.md 缺少 yunying_usage_rules**：补充该工具的文档（System 类别）。
- **CHANGELOG 0.6.1 重复条目**：移除与 0.6.0 重复的 "HOME/USER/LOGNAME 环境变量" fix。

### Changed
- **README/README.zh 文档索引**：补充 connect-design.md 和 terminal-state-design.md 设计文档链接。
- **CONTRIBUTING.md**：Rust 版本要求从 "1.85+" 改为 "stable (see rust-toolchain.toml)"。
- **CHANGELOG 0.6.1 措辞**：CI 移除描述改为"移除 GitHub Actions CI 测试 workflow（保留 release workflow）"。

## [0.7.0] — 2026-07-28

### Changed
- **依赖更新**：rmux-sdk 0.9.0→0.9.1（安全加固 + pane 滚动条 + copy-mode 行号，wire v5 不变，daemon 0.9.0 兼容）、tokio 1.52→1.53、clap 4.6.1→4.6.4、rustls 0.23.41→42、futures 0.3.32→33 等 ~70 个传递依赖。
- **QUIC 拥塞控制 Cubic→BBR**：MCP 端与 Bridge 端统一启用 BBR 拥塞控制算法，替代 quinn 默认的 Cubic。在有丢包的链路上 BBR 基于带宽和 RTT 模型调速，不因少量丢包大幅降窗。同时增大 QUIC 流控窗口至 16MB（`stream_receive_window`/`send_window`/`receive_window`），消除默认 ~12KB 初始窗口的慢启动瓶颈。MCP 端抽取 `build_transport_config()` 统一 3 处连接函数的传输配置。200MB 上传从 60.4s 降至 28.1s（+53%），1GB 稳态吞吐 82 Mbps（链路利用率 82%）。
- **文件下载 SHA256 改为接收方计算**：下载协议变更——Bridge 端不再预计算 SHA256（原先需完整读取文件一遍算 hash 再读一遍传数据），改为只流式发送 `status + file_size + data`；MCP 端边接收数据边计算 SHA256。Bridge 磁盘 I/O 减半，与上传路径统一为"接收方算 hash"模式。200MB 下载从 63.5s 降至 21.8s（+192%）。⚠️ 需 MCP 与 Bridge 同步升级。
- **Bridge 端文件传输 buffer 8KB→1MB**：`download_file_quic`/`download_dir_quic` 中 `tokio::io::copy`（默认 8KB buffer）替换为 `copy_with_buf`（1MB，与 MCP 端 `COPY_BUF_SIZE` 对齐），syscall 次数减少 128 倍。
- **ProgressReporter 支持 Clone**：新增 `Clone` impl（共享 `Arc<Mutex<Stdout>>` writer，独立节流定时器），移除 `noop()`。并发文件操作（目录上传、batch_upload/download、deploy_bridge）中每个 tokio task 持有独立的 reporter clone，解决并发场景无法发送进度通知的问题。

### Fixed
- **并发文件操作触发 MCP 客户端超时**：`upload_dir`、`batch_upload`、`batch_download`、`deploy_bridge` 使用 `ProgressReporter::noop()` 导致长时间传输无进度通知，MCP 客户端（如 opencode）因无响应超时断开。修复：所有文件传输工具统一传入 `ProgressReporter`，每个并发任务 clone 独立 reporter，确保进度通知持续发送。

## [0.6.2] — 2026-07-27

### Added
- **pane_id 自动探测**：23 个 MCP 工具的 `pane_id` 参数改为可选。省略时，server 复用当前 QUIC 连接查询 `list_window_panes(window 0)`，自动选择编号最小的 pane。响应中返回 `resolved_pane_id` 字段（自动探测时附 `auto_resolved: true`），消除 AI 客户端每次操作前的 `list_window_panes` 前置调用。破坏性工具（`close_pane`、`paste_buffer`、`respawn_pane`）仍要求显式指定 `pane_id`。
- **`--idle-timeout-secs` CLI 参数**：交互式控制流空闲超时（秒），超时后断开客户端并恢复 pane 布局。默认 28800（8 小时），设为 0 禁用。可通过环境变量 `IDLE_TIMEOUT_SECS` 覆盖。
### Fixed
- **CLI 异常断连导致 pane 布局损坏**：当客户端异常断开（网络中断、进程被 kill 等），attach 时设下的 pane 尺寸未被恢复，导致同窗口其他 pane 被挤压至 1 行高度。修复：bridge 检测 QUIC→PTY 数据流异常终止后，自动调用 `select-layout even-vertical` 恢复窗口布局。

### Changed
- **交互式控制流新增 idle 超时检测**：8 小时内无操作自动断开连接，避免僵死连接长期占用资源。
- **serde_yaml → serde_yml**：`serde_yaml` 上游已废弃，迁移至 `serde_yml` crate，无功能变更。

## [0.6.1] — 2026-07-24

### Fixed
- **deploy_bridge 耗时 47s**：`systemctl restart` 后 `exec_in_session` 在已断开的 QUIC 连接上等待 sentinel，直到 30s idle timeout 才返回。修复：restart 步骤改为 fire-and-forget `send_keys`，不等 sentinel，重启后状态由重连后的单独验证步骤确认。部署耗时从 47s 降至 4s。

### Added
- **initialize instructions 增加 Core Concepts**：MCP initialize 响应与 SKILL.md 补充核心概念说明（bridge 架构、session 共享模型、工具选择规则、错误处理规则）

### Changed
- **移除 GitHub Actions CI 测试 workflow**（保留 release workflow）：本地 `just check`/`just lint`/`just test` 替代提交级门禁
- **清理死代码**：CLI、bridge、MCP crates 中未使用的函数与模块
- **删除不可用命令与幽灵文件**：justfile 移除损坏的 `run-bridge`/`run-mcp` recipe；删除无脚本引用的 `deploy/rmux-bridge.service`（实际部署由 install-bridge.sh heredoc 生成），release.yml 打包同步移除
- cargo fmt 全量格式化

## [0.6.0] — 2026-07-22

### Fixed
- **新创建 session 缺少 HOME/USER/LOGNAME 环境变量**：bridge 作为 systemd 服务运行，进程环境不包含这些变量，导致 pane 的 bash 仅将其设为 shell 变量而未 export。Go 静态编译的 kubectl 调用 `os.UserHomeDir()` 返回空，回退到相对路径 `.kube/config`，在 `~/.kube/` 目录下找不到文件。修复：`session.rs` 创建 session 时通过 `ProcessSpec.environment` 传递从 `getpwuid` 获取的用户环境，并用 login shell 补齐完整 PATH。

### Added
- **结构化错误信封**：`tools/call` 业务失败统一走 result（`ok:false` + `error` 原字符串 + 新增 `error_code`/`recovery_hint`/`retryable`）并标记 `isError: true`，替代原来分裂的 JSON-RPC `-32000` 通道——错误内容稳定进入模型上下文，Agent 可凭错误码可靠分支。错误码覆盖主机/会话/pane/窗口/隧道未找到、参数缺失、路径穿越、白名单拒绝、认证失败、bridge 不可达、连接丢失、超时等；exec 安全拒绝标记为 `REFUSED_STATE`。未知工具仍按 MCP 规范返回 `-32602`。SKILL.md 错误对照表与 initialize instructions 已同步教授新规则（按 error_code 分支、retryable:false 禁止盲目重试）
- **操作审计追踪**：Bridge 侧 PTY 全量录制（asciinema v2）+ 连接事件 SQLite + MCP 定期拉取录制文件到本地
- **新 MCP 工具**：`query_bridge_audit`（查询 bridge 侧事件日志）、`list_recordings`（列出已同步录制）、`get_recording`（获取录制内容）
- **CLI replay 子命令**：`yunying-cli replay <file.cast> [--speed N] [--idle N]` 本地回放录制
- **审计闭环**：审计查询/清理/配置重载操作本身也被记录（AuditAction 新增 5 个变体）

## [0.5.0] — 2026-07-20

### Added
- **AI 面板自动贴底滚动**：流式输出时视图自动跟随最新消息；向上翻页（↑/PageUp/滚轮）进入回看模式，翻回底部自动恢复跟随
- **AI 思考动画标识**：静态 "AI thinking..." 替换为 braille 旋转动画 + 已等待秒数（如 `⠹ AI 思考中… 7s`）

### Fixed
- **exec 成功路径不返回 `terminal_state`/`cursor`**：与文档承诺不符（只有拒绝路径才带）。`capture_window` 现在透传 bridge 返回的这两个字段，等待循环记录最后一次 capture 的值并随成功结果返回
- **Ghostty 中 `connect` 后键盘卡死**（输出正常、输入无响应）：PTY 模式从 crossterm 事件解析改为**原始字节透传**。原"解析成事件再重新编码"的实现会吞掉远端等待的终端应答序列（如 `\x1b[?997;2n`），且 crossterm 解析器遇到 Ghostty 特有序列会停摆。现在 stdin 字节直接转发远端，仅拦截 `Ctrl+G`/`Ctrl+\`/`Ctrl+L` 控制字节，resize 改用 SIGWINCH；鼠标模式改由远端应用拥有，CLI 不再写鼠标序列、不再翻译鼠标事件。同步移除 `translate_mouse_event` 与 `keymap` 模块
- **opencode serve 抢占终端输入**：spawn 时补 `stdin(Stdio::null())`，避免子进程继承终端 stdin 与 CLI 抢输入

## [0.4.1] — 2026-07-20

### Changed
- **升级 rmux-sdk 0.8→0.9**：wire v3→v5，需 daemon 0.9+ 配套

### Fixed
- **CLI 连接后按键无效**：rmux 0.9 将 `allow-passthrough` 默认改为 `off`，bridge PTY attach 时按键被 daemon 拦截。新增 `config/rmux.conf` 模板，部署时写入 `set -g allow-passthrough on`
- **Windows CLI 编译**：`#[cfg(unix)]` 条件编译包裹 AI 面板的 stderr 抑制逻辑

### Added
- `config/rmux.conf` daemon 配置模板（mouse、history-limit、allow-passthrough）
- Release 包包含 `rmux.conf`

## [0.4.0] — 2026-07-19

### Added
- **AI 对话面板**：`connect` 会话内按 `Ctrl+G` 唤起，基于 `opencode serve`（端口 14096），SSE 实时流式输出；支持 `@analyze`（分析当前终端内容）和 `@clear`（清空对话）；AI 可通过 question 机制向用户提问
- **opencode serve 生命周期管理**：首次提问自动启动，面板关闭/重开不杀进程，CLI 退出自动清理；新增 `--opencode-dir` 指定 AI 工作目录
- **PTY 透传鼠标支持**：SGR (1006) 编码转发鼠标滚轮与触摸板手势

## [0.3.0] — 2026-07-15

### Changed
- **代码组织重构**：大文件拆分，提升可维护性
  - `tools.rs` (3661行) → `tools/` 目录 12 个子模块
  - `protocol.rs` (1965行) → `protocol/` 目录 6 个子模块
  - `main.rs` (1163行) → `handler.rs` + `audit_cli.rs` + `schema.rs`
  - `router.rs` `RwLock::unwrap()` 改为 `expect()`，防止 poisoned lock panic
- **Exec 输出不再过滤 prompt 行**：`exec` 返回的 `output` 现在包含 start_marker → sentinel 区间的完整终端上下文（含 shell 提示符、命令回显），不做行级过滤——所见即所得。旧版本会过滤掉提示符和命令回显行
- **Exec 等待机制从轮询改为事件驱动**：命令发送后通过 bridge 端 `wait_for_text` 阻塞等待 sentinel 标记出现，替代旧的 MCP 侧轮询 `capture_pane` 方式，降低网络开销和延迟
- **Exec 默认超时从 30s 增加到 600s（10 分钟）**：适应编译、包安装等较长时间命令，仅作为安全兜底。正常命令无需手动设置 `timeout_ms`

### Added
- **配置热加载**：支持在不重启 MCP Server 的情况下重新加载 `hosts.yaml` 配置
  - 新增 `reload_config` MCP 工具，AI Agent 可主动触发
  - 支持 SIGHUP 信号触发（`kill -HUP <pid>`），运维友好
  - 加载失败时保留原有配置，不影响运行中服务

### Security
- **文件路径穿越防护**：Bridge 端 `file_upload`/`file_download` 拒绝包含 `..` 的路径，防止路径穿越攻击
- **下载目录遍历防护**：MCP 端验证远端返回的相对路径不含 `..` 且非绝对路径
- **隧道目标白名单（SSRF 防护）**：`hosts.yaml` 新增可选 `allowed_tunnel_targets` 字段，支持 glob 模式限制端口转发目标（如 `"127.0.0.1:5432"`、`"10.0.1.*:*"`），不配置则全部允许（向后兼容）

### Added
- **交互式终端直连**：新增 `yunying-cli` crate，提供 `yunying-cli connect` CLI 命令
  - PTY + `rmux attach-session` 子进程透传方案，完美支持 vim/htop 等 TUI 程序
  - QUIC 双流协议（0x06 控制 + 0x07 数据），控制面与数据面分离
  - crossterm raw mode 终端转发，支持 resize/detach
- 部署脚本拆分：`install-daemon.sh`（rmux daemon）+ `install-bridge.sh`（rmux-bridge），职责独立
- `/etc/profile.d/yunying.sh`：自动设置 `RMUX_TMPDIR` 环境变量，用户登录后可直接 `rmux a -t yunying`
- Bridge 请求级别日志：INFO 显示请求摘要（type/session/duration），DEBUG 显示完整请求/响应 JSON
- Bridge `--log-level` 参数（默认 `info`，支持 trace/debug/info/warn/error，可通过 `RUST_LOG` 环境变量覆盖）
- 端口转发功能：`tunnel_create`、`tunnel_list`、`tunnel_close` 三个 MCP 工具
  - 通过 QUIC 隧道访问远程内网服务（数据库、API 等）
  - 1 小时空闲超时 + 15 秒 keepalive，适合长连接场景
  - 64KB 缓冲区，支持 TCP 半关闭处理
  - 完整审计日志记录
- **终端状态感知**：新增 `terminal_state.rs` 模块，`TerminalState` 枚举覆盖 8 种终端状态（ready/running/password/confirm/repl/editor/pager/unknown）
  - 5 个工具（`capture_pane`、`exec`、`wait_for_text`、`wait_stable`、`pane_info`）新增 `terminal_state` 和 `cursor` 返回字段
  - 22 个单元测试覆盖启发式检测逻辑
- **Exec 执行前安全检查**：`exec` 执行命令前检测终端状态，非 `ready` 状态自动拒绝执行
  - 新增响应字段：`pre_terminal_state`（执行前状态）和 `refused`（是否被拒绝）
  - 防止命令注入到 vim/less/密码提示/REPL 等非 shell 上下文
  - 向后兼容：检测失败时正常执行，不影响已有用法

### Removed
- **移除 TCP/TLS 回退传输**：MCP 与 Bridge 之间仅使用 QUIC 协议，删除 TCP listener、yamux 多路复用、`proxy_legacy`、`BridgeStream::Tcp` 等约 700 行代码。移除 `tokio-rustls`（MCP 侧）和 `tokio-yamux`（Bridge 侧）依赖
- `--insecure` 参数完全禁用，`--ca-cert` 改为必填（H-03 高危安全风险：消除 MITM 攻击面）

### Changed
- **rmux-sdk 从 0.7 升级到 0.8**（wire protocol v3，需同步升级 daemon）
- 移除 JWT 认证支持，认证简化为纯静态 token 常量时间比较
- 工作空间依赖统一到根 Cargo.toml，13 个共享依赖版本集中管理
- `rmux-bridge.service` 添加 `After=rmux-daemon.service` 和 `Requires=rmux-daemon.service`
- 部署脚本 socket 检测路径增加 `$HOME/.rmux`（与 daemon service 的 `RMUX_TMPDIR` 保持一致）
- Bridge CLI `--rmux-socket` 默认值保持 `/tmp/rmux-1000/default`，部署脚本自动检测实际路径（含 `$HOME/.rmux`）并通过参数传入

### Security
- host_filter 通配符过滤从手写正则改为 `glob::Pattern`，消除 ReDoS 风险
- Exec 执行前终端状态检查：防止在 vim/less/密码提示/REPL 等非 shell 上下文中注入命令，避免意外数据修改或信息泄露

## [0.1.0] — 2026-07-02

### Added
- 39 MCP 工具（38 可用 + 1 开发中 `stream_pane`）
- 3 个批量操作工具：`batch_exec`、`batch_upload`、`batch_download`（多主机并发执行/上传/下载）
- QUIC 协议传输（UDP :9788）
- CA 签发 + 按主机独立证书的多主机 PKI 体系
- Windows/macOS/Linux 客户端原生支持
- Bridge 并发连接限制（`--max-connections`，默认 256）
- Token 认证，恒定时间比较
- SQLite 审计日志（query/stats/cleanup）
- 主机注册表（group/tag/label 过滤、broadcast_keys）
- 文件传输：QUIC 上传/下载、目录递归并发上传
- systemd 服务部署 + `just deploy` 一键部署
- `--insecure` 标志用于调试环境跳过 TLS 验证
- 审计 CLI 子命令（`audit query/stats/cleanup`）

### Fixed
- 生产代码 `unwrap()` 改为 poison-safe 模式
- Bridge QUIC handler 支持 JSON RPC 终端操作
