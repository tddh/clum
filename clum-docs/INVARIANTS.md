# clum 设计不变量（INVARIANTS）

> 目的：把"为什么这样设计"从脑子里挪到纸面上。每条三段式：**约定** → **理由** → **何时需要重新审视**。
> 新增功能时对照本清单；修改任何一条前，先更新这里的理由。
>
> 基准：v0.17.1 + 2026-09-13 评审修复批次。事实陈述均经代码验证。

---

## 1. 身份与 bootstrap

**约定**：无 group 的 API Key = 超管；**API key store 为空（从未 `agent add`）时服务器处于 bootstrap 模式**——回环连接（127.0.0.1 / ::1）视为超管（供服务器本机完成初始化），非回环请求一律拒绝，直到创建首个 key。stdio 本地模式天然等同超管。

**理由**：首次部署的鸡蛋问题——必须先连上服务器才能创建第一个 key。设计正确性依赖**部署边界**（端口不暴露给不可信网络），不依赖代码。自 2026-09-13 起由机制强制（非回环不再自动获得超管身份）。

**重审时机**：若未来引入多实例/远程初始化流程（如初始化 token、OAuth bootstrap）。

## 2. exec 与 shell_command 是两条不同的通道

**约定**：`exec` 走**键盘通道**（send_keys 注入 marker+命令+sentinel，在 shell 里交互执行，等待并捕获输出）；`shell_command` 走**进程通道**（`pane.shell()` → `/bin/sh -c`，替换 pane 进程，异步、无输出）——**且只接受 dead pane**（见 §3 实测核准）。

**理由**：两种进程生命周期管理范式（同步请求-响应 vs 异步任务+监控）。两通道的**威胁模型不同**：exec 的控制字符校验与 terminal_state fail-closed 门控防的是"命令经键盘通道被拆行/注入交互程序"；shell_command 不经键盘，无此注入面。**不要给两者套同一套门控。**

**重审时机**：若 shell_command 增加"阻断式"语义（等待/捕获），或键盘通道引入新的注入变体。

## 3. rmux daemon 拒绝替换运行中进程（底层兜底）

**约定**：`shell_command` / 无 `kill=true` 的 respawn 在 pane 有前台进程时，被 rmux daemon 以稳定消息拒绝：`"pane still active; use -k to force respawn"`（rmux-proto `PANE_STILL_ACTIVE_MESSAGE`），分类为 `PANE_BUSY`。**"静默杀掉运行中程序"在 clum 中不可能发生。**

**2026-09-13 生产实测核准（两点精确化）**：
1. **idle 交互 shell 也被视为"运行中进程"**——因此 `shell_command` 的实际适用面是 **dead pane**（`keep_alive_on_exit` 保留的已退出 pane），而**不是**文档早期所说"空闲 shell"；要替换活进程只能 `respawn_pane(kill=true)`。
2. **`respawn_pane` 无 `command` 参数时原样重跑该 pane 当前的进程规格**（`split_pane_with(sleep 300)` 起的 pane 会再跑 `sleep 300`），不会回退到"默认 shell"——要起新内容必须显式传 `command`。

**理由**：SDK 默认 `kill_existing=false`；clum 调用链未传入 kill。schema 描述必须陈述此确定行为，不得写"depends on the rmux daemon"。

**重审时机**：升级 rmux-sdk 0.11+ / wire 9 时验证此语义未变。

## 4. session_create 幂等（CreateOrReuse）

**约定**：`session_create` 对已存在会话幂等——复用并返回首个 pane id，**不报 SESSION_EXISTS**。`SESSION_EXISTS` 错误码对 MCP 路径不可达，仅为错误码"只增不改"契约保留。README/schema 一律按幂等语义表述。

**理由**：幂等对 AI 调用方更优（省一次探测往返）。语义契约以 `rmux-bridge/src/protocol/session.rs` 的 `EnsureSessionPolicy::CreateOrReuse` 为准。

**重审时机**：若引入显式"严格创建"需求（如并发初始化竞态检测），新增参数而非改变默认。

## 5. 错误码契约：只增不改

**约定**：`error_code` 是稳定契约——**只增不改**；`error` 字段保留原始字符串；`recovery_hint` + `retryable` 由 MCP 侧补齐（`enrich_error` 幂等，可重复调用）。retryable=true 白名单仅三个：`BRIDGE_UNREACHABLE` / `CONNECTION_LOST` / `CONNECT_TIMEOUT`；`TIMEOUT`（命令超时）显式不可重试——进程仍在远端跑。分类器（`clum-core/error_code.rs`）为 MCP 与 bridge 双端共用。

**理由**：AI 消费方依赖错误码做重试决策；改码即破坏所有下游。recovery_hint 为英文（与 instructions/工具描述统一，2026-09-13 起）。

**重审时机**：新增错误场景 = 新增码 + 两端测试；任何"重命名/合并错误码"的念头一律停下。

## 6. 审计/录制类工具必须组隔离（GROUP_SCOPED_TOOLS）

**约定**：工具面已冻结（§13），本条的"新增义务"仅在解冻时生效——若未来新增访问审计或录制数据的工具（当前：`audit_query` / `list_recordings` / `get_recording` / `search_recordings`，见 `tools/mod.rs` 的 `GROUP_SCOPED_TOOLS`），必须经 `hosts_in_group` 强制 caller-group 过滤，**且必须覆盖"不传 host 参数"的场景**。注意：`GROUP_SCOPED_TOOLS` 是文档锚点 + debug 自检，**不是运行时拦截**——组过滤的实际强制在四个工具各自的分发分支内。

**理由**：历史教训——search_recordings 曾遗漏，受限 key 不传 host 即可跨组解密检索全部录制（2026-09-13 修复）。authorize() 只校验 args 里显式出现的 host/hosts，可选 host 参数不能被当作授权边界。

**重审时机**：解冻新增审计/录制/回放类工具时走这张清单；组语义变化（如多组归属）时全量回归四个工具。

## 7. QUIC-only 传输

**约定**：Server↔Bridge、Server↔CLI 数据面仅 QUIC（UDP），**无 TCP/WebSocket 回退**。

**理由**：多路复用/低延迟/单连接设计收益；已知代价——企业/运营商防火墙封锁 UDP 时不可用，部署前需确认 9788/udp 可达性。这是一个**环境假设**，不是能力缺口；引入回退前先收集真实受阻部署证据。

**重审时机**：出现多个真实"UDP 被封锁"部署案例时，评估 WebSocket 回退通道（代价：新增一条数据面路径与攻击面）。

## 8. 审计写入当前是 fail-open（已知债务）

**约定**：审计写库失败仅记日志、不阻断操作（`audit/log.rs`）——**审计不是强制门**。防篡改双层：中央审计库为前向哈希链（见下），录制侧 chattr +a。

**2026-09 哈希链增量**：每条审计写入携带 `entry_hash = SHA256(prev_hash ‖ payload)`（payload 为 14 值列长度前缀串接，`chain.rs` 单一实现供写入与校验两侧共用）；`clum-mcp audit verify` 全链重算，篡改/删除可检测（对被篡改数据报告 BROKEN 而非 panic）。cleanup 删除哈希行前在 `audit_chain_checkpoints` 记录断点（管理删除承认机制）；自检 warn 为事后检测，不能恢复已删数据。链保证"存在的内容未被改"，不保证"该发生的都已记录"——后者仍是下述 fail-open 债务。chain head 打印在 verify 输出中，供外置比对；外置自动化（WORM/objstore）为后续批次。

**理由**：fail-open 当前取舍是可用性优先。但项目以"审计即卖点"，此债务与叙事冲突。

**重审时机**：任何合规/企业部署需求出现时，优先级提到 P1（fail-closed 选项或双写告警）。

## 9. 敏感输入：审计脱敏 ≠ 录制脱敏

**约定**：redaction 只作用于**审计 DB**（password 态无条件、`sensitive` 强制）；PTY 录制（asciinema）**逐字节记录原始输入**——密码在录制内容中是明文，仅受静态加密保护（X25519 + AES-256-GCM；直接模式无服务器公钥时**明文落盘并告警**，fail-open 路径）。运维指引：生产主机走 NOPASSWD/密钥，不让密码进终端。

**理由**：录制的内容级掩码与终端语义冲突（录制必须忠实时序回放）；边界在 SECURITY.md 如实披露。

**重审时机**：若引入录制内容掩码技术（按 password 态抑制字节），或静态加密 fail-open 路径被判定不可接受。

## 10. 执行状态存于远端（断线存活的根基）

**约定**：exec 的 marker/sentinel 状态写在**远端 pane 屏幕**上，不在服务器/客户端内存。断连重连续等同一命令；bridge 完全无状态。

**理由**：这是"持久会话"承诺的机制保证——任何把执行状态搬进 MCP server 内存的"优化"都会破坏断线存活，一律拒绝。

**重审时机**：几乎不需要。此条是项目根基。

## 11. 单实例部署边界

**约定**：中央服务器设计为**单实例**：BridgeRegistry / 转发表 / stream 状态全内存，无分片与选主。重启后 bridge 自动重连，但进行中的 stream/forward 丢失。

**理由**：当前目标用户（单团队）下单实例足够；多实例会引入一致性层，复杂度不成比例。

**重审时机**：出现 HA/横向扩展需求 → 先做"连接信息落库 + 快速冷启"，再谈多活。

## 12. 外部依赖集中点

**约定**：`rmux-sdk`（0.10，wire 8）仅由 `rmux-bridge` 依赖；MCP 与 CLI 不感知 rmux 细节。SDK 升级历史上发生过硬切断（wire 5→8 会话全丢），升级窗口需整链同步。

**理由**：把外部变更半径限制在单一 crate 内。

**重审时机**：rmux-sdk 任何小版本升级前，先核对 wire 版本与 ensure/session 语义。

## 13. 工具面冻结（68 个，不再扩张）

**约定**：MCP 工具数量冻结在 **68** 个（2026-09-13，移除 session_detach 后的产品决策）。后续演进以质量与减法为主线：打磨现有工具的契约与描述，必要时合并/删除薄特化对（候选：`find_pane_text`/`find_text_all`、`capture_pane`/`capture_region`、`get_pane_title`/`get_pane_by_title`，见 2026-09-13 全面评审 §2.3），而非纵向新增。

**理由**：工具表面积 = schema + 测试 + 文档 + "要记住的点"的持续开销。69→68 的减法已经验证了"删除窗口在关闭"；对单人维护者，每新增一个授权矩阵、组隔离清单、错误分类的对照点都是新的记忆负担。

**重审时机**：出现确有生产缺口的候选工具时，先问能否用现有工具组合表达、能否以"删一换一"净零增长替代；确需净增时必须同步补全：GROUP_SCOPED_TOOLS 对照（§6）、错误码契约、TOOLS.md、README×2、SKILL.md、行为测试。

---

## 不再重复踩的坑（本清单的来历）

- [2026-09-12] `search_recordings` 缺组隔离 → 跨组越权（组过滤必须覆盖可选 host 参数的缺省场景）→ 第 6 条。
- [2026-09-12] 把 schema 模糊文案（"depends on the rmux daemon"）当事实，误判 shell_command 会静默杀进程 → 未查 rmux-sdk 实现（`kill_existing=false`）→ 凡"声明 vs 行为"存疑，先读实现（第 3 条）。
- [2026-08] `SESSION_EXISTS` 契约漂移：schema 说"已存在报错"，实现是 CreateOrReuse 幂等 → 第 4 条 + 文案修正。
