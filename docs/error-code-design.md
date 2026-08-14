# clum MCP 错误码系统：问题确认与修复方案

> 状态：已评审通过，进入实施
> 日期：2026-08-14
> 范围：clum-core（共享分类器）、clum-mcp（MCP 侧分类器与信封）、rmux-bridge（桥侧错误注入）、docs/TOOLS.md（文档同步）
> 原则：错误码**只增不改**——`error` 保留原始字符串，`error_code` 是稳定契约，`recovery_hint` 给恢复动作，`retryable` 给重试决策

---

## 1. 背景与现状

所有 MCP 工具调用统一返回错误信封：

```json
{ "ok": false, "error": "...", "error_code": "XXX", "recovery_hint": "...", "retryable": false }
```

产生链路：

```
工具函数返回 ok:false / anyhow 错误
  → http_server.rs:137-158 / handler.rs:49-75（统一入口）
  → enrich_error()（业务失败补字段，幂等） / error_result()（anyhow 展开）
  → classify_message()（基于错误消息子串匹配分类）
```

**根本局限**（error.rs:6 注释自认）：bridge 端不携带结构化错误码，所有分类依赖"消息子串猜测"。经审计，bridge 端约 60+ 种错误消息模板中**约 83%（50 种）无法被当前分类器命中**，落入 `UNKNOWN`。

---

## 2. 问题清单（全部已确认，含证据）

### P0-1：`ok:true` + `error` 字段语义矛盾（5 处）

| 位置 | 工具 |
|------|------|
| `batch.rs:26-31` | batch_exec |
| `batch.rs:210-213` | batch_upload |
| `batch.rs:329-332` | batch_download |
| `batch.rs:457-460` | batch_send_keys |
| `deploy.rs:59-62` | deploy_bridge |

代码模式：

```rust
return Ok(json!({"ok": true, "total": 0, "success": 0, "failed": 0,
    "total_duration_ms": 0, "results": {}, "error": "empty hosts list"}));
```

**问题**：`ok:true` 却带 `error` 字段。`enrich_error()` 只在 `ok == false` 时处理（error.rs:135），此处的 error 永远不会被分类、永远无人消费——AI 客户端看到 `ok:true` 即视为成功，忽略 error。空 hosts 列表实为参数错误，应 `ok:false`。

### P0-2：`resolve_pane_id` 的"透传"实为"重分类"（common.rs:120-129）

```rust
// 注释声称"透传 bridge 原始错误（SESSION_NOT_FOUND 等）"
let code = resp["error_code"].as_str().unwrap_or("UNKNOWN");
let msg = resp["error"]...;
anyhow::bail!("[{}] {}", code, msg);   // 实际产出 "[UNKNOWN] session not found: clum"
```

**问题**：
- bridge 端**从不返回 `error_code` 字段**（rmux-bridge 全 crate grep 0 匹配），`code` 恒为 `"UNKNOWN"`——注释与实际行为不符
- 包装成 `[UNKNOWN] msg` 后经 `error_result()` **重新分类**。当前靠子串匹配碰巧命中原始关键词；但将来桥侧若真带 error_code（error.rs 预留了此方向），这里会**丢弃桥侧码再猜一遍**
- 修复应直接透传原始 `error` 消息，不包裹前缀

### P1-3：分类器漏匹配 —— `invalid pane_id`（bridge 最高频盲区）

bridge 端 **30+ 处**返回 `"invalid pane_id: %X"`（pane.rs / exec.rs / output.rs / buffer.rs，含 `invalid source_pane_id` / `invalid target_pane_id`），但分类器 PANE_NOT_FOUND 规则只认：

```rust
if has("pane id") && has("not found") || has("can't find pane") || has("pane not found")
```

- `"pane id"`（带空格）+ `"not found"` 组合 **bridge 从不产生**（bridge 用下划线 `invalid pane_id`）——该子句是死分支
- PANE_NOT_FOUND 实际仅靠 `"pane not found"` 命中（bridge `pane.rs:277` `"pane not found in info snapshot"`，仅 1 处）
- 现有测试 `"pane id %99 was not found"` 是**虚构格式**，与 bridge 实际消息不符
- 后果：绝大多数 pane 参数错误落 `UNKNOWN`

### P1-4：`host 'xxx' not found in enrolled bridges` 漏匹配

`discovery.rs:221` 返回 `"host '{}' not found in enrolled bridges"`——`host` 与 `not found` 之间插了主机名，不构成 `"host not found"` 连续子串 → 落 `UNKNOWN`。

### P1-5：连接超时被归为 `TIMEOUT` 且 `retryable:false`（语义相反）

| 消息 | 产生点 | 当前分类 | 应有语义 |
|------|--------|----------|----------|
| `"QUIC connect timeout"` | `transport.rs:87` | TIMEOUT, false | 连接失败，**可重试** |
| `"TCP connect timeout"` | bridge `files.rs:393` | TIMEOUT, false | 连接失败，**可重试** |

`classify_message` 的 `has("timeout")` 匹配面过宽，无法区分"命令执行超时"（exec sentinel 未出现，不重试）与"连接超时"（应重试）。

> 修正此前误判：`exec.rs:707` 的 `"connection lost and reconnect failed"` 因 CONNECTION_LOST（第 14 位）先于 TIMEOUT（第 15 位）匹配，实际归 CONNECTION_LOST（retryable:true）——**此条无需修复**。

### P1-6：`AUTH_FAILED` 触发路径与文档描述不符

- 文档（TOOLS.md）写"bridge token 不匹配"，但 enrolled 模式下 token 校验在 `quic_server.rs:222`（`"invalid token"`）、`quic_server.rs:426`（`"invalid api key"`）——均为**协议层断连**，不经过 `enrich_error`，不产生此码
- 实际触发路径：**direct 模式**下 MCP 作为客户端直连 bridge，bridge 端 `auth.rs:51` 认证失败返回 `"authentication failed"` → 冒泡到工具层 → 命中 AUTH_FAILED
- 结论：分类器分支**保留**（direct 模式有效），但文档描述需修正为"direct 模式下 bridge token 认证失败"

### P2-7：`INVALID_PARAMS` 覆盖不全

- 命中：106 处 `.context("missing 'xxx'")` → `"missing '"` 模式
- 漏掉：`"invalid direction: ..."`（bridge pane.rs:102）、`"rows and cols must be non-zero"`、`"all 4 coordinates required"`、`"stable_ms must be positive"`、`"cols/rows/width/height must be 0-65535"`（proxy.rs）、`"invalid base64"`、`"unknown layout: ..."`、`"path must be absolute"`（mod.rs:444）——全部落 UNKNOWN

### P2-8：deploy_bridge 双轨状态（`status` 与 `error_code` 并存）

`deploy.rs` 返回 `{ok:false, status:"first_time_deploy"}` 等 8 种小写 status，同时 `enrich_error` 还会基于 error 消息补一个 error_code。TOOLS.md 的 deploy 章节有 status 表，但通用错误码表未说明二者关系——调用方需知道"deploy 场景看 status，其余看 error_code"。

### P2-9：bridge 帧层错误格式不一致（`{"error": ...}` 无 `ok:false`）

`proxy.rs` 5 处帧层错误用 `{"error": "..."}`（无 `ok` 字段）：`invalid json`（:66）、`unknown request type`（:615）、`audit query failed`（:100）、`audit stats failed`（:116）、`list_unsynced failed`（:127）。`enrich_error` 检查 `ok == Some(false)` 不满足 → **完全不分类**，AI 拿到无 error_code 的裸错误。

### P2-10：bridge 其余 30+ 种消息落 UNKNOWN（按类别聚合）

除上述外，bridge 还有这些类别无法命中（agent 审计清单）：

| 类别 | 消息示例 | 建议归属 |
|------|----------|----------|
| CLI 回退失败 | `"rmux CLI failed: ..."`、`"CLI command 'X' exited with code N: ..."`（9 处） | CLI_FAILED（新码） |
| pane 无 id | `"split pane has no id"`、`"pane has no id"`（3 处） | PANE_NOT_FOUND |
| 标题查找失败 | `"no pane found with title: ..."`（1 处） | PANE_NOT_FOUND |
| 路径安全 | `"path contains null byte"`、`"directory too deep (>64)"`（2 处） | PATH_TRAVERSAL |
| 协议错误 | `"frame too large"`、`"invalid json"`、`"unknown request type"`（3 处） | PROTOCOL_ERROR（新码） |
| 认证协议 | `"invalid auth preamble"`、`"token too long"`（2 处） | AUTH_FAILED |
| 注册失败 | `"registration rejected"`、`"QUIC handshake failed"`（2 处） | BRIDGE_UNREACHABLE |

---

## 3. 修复方案

### 3.1 分类器增强（`crates/clum-mcp/src/error.rs`）

**新增错误码定义**（遵循"只增不改"）：

| 新 error_code | 语义 | retryable | recovery_hint |
|---------------|------|-----------|---------------|
| `CONNECT_TIMEOUT` | 连接超时（bridge 无响应/不可达） | **true** | 确认主机在线、Server 9788 端口可达后重试 |
| `CLI_FAILED` | bridge 端 rmux CLI 回退失败 | false | 检查 rmux 安装完整性（`rmux list-commands`） |
| `PROTOCOL_ERROR` | 桥侧帧协议错误（不应发生） | false | 检查 bridge 版本是否过旧，考虑升级 |

**classify_message 新增/修正匹配**（保持优先级从具体到宽泛）：

| 顺序 | 匹配模式 | 结果码 |
|------|----------|--------|
| 前置 | `"connect timeout"` / `"connect timed out"` / `"connection timed out"` | CONNECT_TIMEOUT |
| 前置 | `"invalid pane"`（覆盖 pane_id/source_pane_id/target_pane_id） | PANE_NOT_FOUND |
| 中段 | `"not found in enrolled"` | HOST_NOT_FOUND |
| 中段 | `"no pane found with title"` / `"pane has no id"` | PANE_NOT_FOUND |
| 中段 | `"null byte"` / `"directory too deep"`（并入现有 path traversal 分支） | PATH_TRAVERSAL |
| 中段 | `"invalid auth preamble"` / `"token too long"`（并入 auth 分支） | AUTH_FAILED |
| 中段 | `"registration rejected"` / `"handshake failed"`（并入 connection refused 分支） | BRIDGE_UNREACHABLE |
| 中段 | `"rmux CLI"` / `"CLI command"` | CLI_FAILED |
| 中段 | `"frame too large"` / `"unknown request type"` / `"invalid json"` | PROTOCOL_ERROR |
| 中段 | 参数校验族（锚定）：`"invalid direction"` / `"must be 0-65535"` / `"must be non-zero"` / `"must be positive"` / `"must be absolute"` / `"mutually exclusive"` / `"coordinates"` / `"base64"` / `"unknown layout"` | INVALID_PARAMS |

**注意**：
- 参数值域模式**采用锚定子串**（如 `must be 0-65535` 而非宽泛 `must be`），避免未来新增 `"session must be created first"` 这类状态类错误被误归 INVALID_PARAMS（负例测试已覆盖）
- 修正 error.rs:46 PANE_NOT_FOUND 条件——移除死分支 `has("pane id") && has("not found")`，替换为 `has("invalid pane")`，并加括号明确优先级
- **不删除** AUTH_FAILED / 现有任何分支（direct 模式仍依赖）

### 3.2 信封语义修复（`crates/clum-mcp/src/tools/batch.rs`、`deploy.rs`）

5 处 `ok:true + error:"empty hosts list"` 统一改为：

```rust
return Ok(json!({"ok": false, "error": "empty hosts list"}));
```

（enrich_error 会自动补 INVALID_PARAMS + hint + retryable:false；字段 total/success/failed 省略——调用方以 error_code 分支即可。）

### 3.3 透传逻辑修正（`crates/clum-mcp/src/tools/common.rs:120-129`）

```rust
// 修正后：直接透传 bridge 原始错误消息，由上层分类器基于原始文本分类
if resp["ok"].as_bool() == Some(false) {
    let msg = resp["error"].as_str().unwrap_or("unknown bridge error");
    anyhow::bail!("{}", msg);
}
```

同时删除误导注释（"透传 SESSION_NOT_FOUND 等"→ 说明实为消息分类而非码透传）。

### 3.4 bridge 帧层格式统一（`crates/rmux-bridge/src/proxy.rs`）

5 处 `{"error": "..."}` 补 `ok:false`：

```rust
// :66, :100, :116, :127, :615
json!({"ok": false, "error": ...})
```

### 3.5 文档同步（`docs/TOOLS.md`）

1. 错误码表（L35-49）新增 `CONNECT_TIMEOUT` / `CLI_FAILED` / `PROTOCOL_ERROR` 三行
2. `AUTH_FAILED` 描述修正为："direct 模式下 bridge token 认证失败；enrolled 模式 token 校验失败表现为连接建立失败（BRIDGE_UNREACHABLE/CONNECT_TIMEOUT）"
3. `TIMEOUT` 描述补充："命令执行/等待超时；连接超时见 CONNECT_TIMEOUT（可重试）"
4. `PANE_NOT_FOUND` 描述补充涵盖 `invalid pane_id`
5. 错误码表加注 deploy_bridge 使用独立 `status` 字段（见部署章节状态表）

### 3.6 schema.rs

`instructions()` 中的 error_code 说明（L21）无需修改（契约不变，只增码）。

---

## 4. 修复后错误码契约（完整表）

| error_code | 语义 | retryable | 主要触发源 |
|------------|------|-----------|-----------|
| `HOST_NOT_FOUND` | 主机不在注册表 | false | common.rs resolve_host_config / discovery host_set_meta |
| `INVALID_PARAMS` | 缺少/非法参数 | false | MCP `missing 'x'` 106 处 + bridge 参数校验族 |
| `SESSION_NOT_FOUND` / `SESSION_EXISTS` | 会话不存在/已存在 | false | bridge session/pane 消息 |
| `PANE_NOT_FOUND` | pane 不存在/无效 | false | bridge `invalid pane_id` 30+ 处 + `pane not found` 等 |
| `PANE_BUSY` | pane 非空闲 | false | bridge `pane still active` |
| `WINDOW_NOT_FOUND` | 窗口不存在 | false | bridge `window not found in info snapshot` |
| `FORWARD_NOT_FOUND` | 隧道不存在 | false | ForwardManager |
| `PATH_TRAVERSAL` | 路径穿越/不安全路径 | false | bridge `sanitize_path` + MCP `sanitize_local_path`（Server 侧 local_path 校验）+ null byte + directory too deep |
| `FORWARD_DENIED` | 隧道目标不在白名单 | false | forward.rs allowed_forward_targets |
| `AUTH_FAILED` | 直连认证失败 | false | bridge auth.rs（direct 模式） |
| `FORBIDDEN` | 分组隔离拒绝 | false | authorize() |
| `BRIDGE_UNREACHABLE` | bridge 不可达（refused/注册失败） | true | transport.rs + register.rs |
| `CONNECTION_LOST` | 连接中断 | true | exec.rs / transport.rs |
| `CONNECT_TIMEOUT` | 连接超时 | **true** | transport.rs / bridge files.rs（**新增**） |
| `TIMEOUT` | 命令执行/等待超时 | false | exec sentinel / output 等待系列 |
| `REFUSED_STATE` | exec 安全检查拒绝 | false | MCP `exec` precheck（refused 标记）——**非共享分类器常量**，MCP 侧 `error.rs` 硬编码注入 |
| `CLI_FAILED` | rmux CLI 回退失败 | false | bridge CLI 回退 9 处（**新增**） |
| `PROTOCOL_ERROR` | 帧协议错误 | false | bridge proxy.rs（**新增**） |
| `UNKNOWN` | 未分类兜底 | false | 其余 |

---

## 5. 兼容性与风险

| 项 | 评估 |
|----|------|
| 错误码兼容 | 只增 3 个新码（CONNECT_TIMEOUT/CLI_FAILED/PROTOCOL_ERROR），不删不改现有码，既有客户端分支不受影响 |
| 消息文本兼容 | `error` 字段原始文本不改变；`[UNKNOWN] ` 前缀移除是唯一消息格式变化（P0-2），属内部包装，AI 可见文本变干净 |
| 行为变更 | 空 hosts 从 ok:true 变 ok:false——**有语义影响**，但这是修正 bug，且 AI 对 INVALID_PARAMS 的处理（补参数重试）本就是正确反应 |
| bridge 侧风险 | proxy.rs 5 处加 `ok:false` 字段，协议向后兼容（旧 MCP 忽略未知字段） |
| 测试 | error.rs 现有 14 个单元测试需补充新匹配模式用例；修正 PANE_NOT_FOUND 后现有测试 `"pane id %99 was not found"` 仍应通过（需保留该模式或调整测试） |

> 测试注意：现有 `classifies_terminal_objects` 测试用 `"pane id %99 was not found"` 断言 PANE_NOT_FOUND。若移除 `has("pane id") && has("not found")` 分支，该测试将失败。方案：**保留** `"pane id %99 was not found"` 匹配（它确实是 MCP 侧可能产生的格式），同时**新增** `"invalid pane"` 分支——两者共存。

---

## 6. 测试计划

1. **error.rs 单元测试**：
   - 新增：`"invalid pane_id: %99"` → PANE_NOT_FOUND
   - 新增：`"host 'tf99' not found in enrolled bridges"` → HOST_NOT_FOUND
   - 新增：`"QUIC connect timeout"` → CONNECT_TIMEOUT 且 retryable:true
   - 新增：`"cols must be 0-65535"` → INVALID_PARAMS
   - 新增：`"rmux CLI failed: ..."` → CLI_FAILED
   - 新增：`"unknown request type: xxx"` → PROTOCOL_ERROR
   - 新增：`"path contains null byte"` → PATH_TRAVERSAL
   - 保留：现有全部测试（含 `"pane id %99 was not found"`）
2. **batch/deploy**：空 hosts 返回 `ok:false` 且 error_code=INVALID_PARAMS
3. **集成**：`just check` + `just test` + `just lint` 全绿

---

## 7. 实施顺序

| 步骤 | 内容 | 文件 |
|------|------|------|
| 1 | 分类器增强 + 新码 + 测试 | `error.rs` |
| 2 | 信封语义修复（5 处 ok:true→false） | `batch.rs`、`deploy.rs` |
| 3 | 透传逻辑修正 | `common.rs` |
| 4 | bridge 帧层格式统一 | `rmux-bridge/src/proxy.rs` |
| 5 | 文档同步 | `docs/TOOLS.md` |
| 6 | 验证 | `just check && just test && just lint` |

---

## 8. 治本方案：bridge 协议层携带 error_code（本次实施）

### 8.1 架构：错误分类逻辑下沉到 clum-core 共享

```
clum-core（新增 src/error_code.rs，零依赖纯函数）
  ├── 错误码常量：pub const CODE_*: &str
  └── pub fn classify_error_message(msg: &str) -> &'static str   // 消息 → code 字符串
        │
        ├──→ MCP 侧 error.rs：classify_message() 委托此函数取 code，
        │        本地维护 code → (recovery_hint, retryable) 映射表
        └──→ bridge 侧 proxy.rs：send_response() 出口注入 error_code
                （响应是 ok:false 且无 error_code 时，基于 error 字段分类后插入）
```

- clum-core 被 clum-cli / clum-mcp / rmux-bridge 三方依赖，共享分类器保证**两处分类永远一致**
- `classify_error_message` 只返回 code 字符串（hint/retryable 是 MCP 侧 UX 关心的事，bridge 不需要）

### 8.2 桥侧注入点

`rmux-bridge/src/proxy.rs` 的 `send_response()`（proxy.rs:646）是全部 JSON 响应的唯一出口（9 处调用，含 protocol handler 返回值与 proxy 本地构造错误）。改动：

```rust
async fn send_response(writer: &..., response: &mut serde_json::Value) -> Result<()> {
    // 注入：ok:false 且无 error_code 时，基于 error 文本分类
    if response["ok"].as_bool() == Some(false) && response.get("error_code").is_none() {
        let msg = response["error"].as_str().unwrap_or("");
        response["error_code"] = json!(clum_core::error_code::classify_error_message(msg));
    }
    // ... 序列化发送（不变）
}
```

- 签名 `&Value` → `&mut Value`，9 处调用点同步改 `&mut`
- 覆盖：protocol handler 返回的 `{"ok": false, "error": ...}`、proxy 本地构造的 `frame too large` / `mark_synced failed` / stream_subscribe 错误
- **不覆盖**：stream_subscribe 二进制流（0x02）、interactive 控制流（0x06）——这些非 JSON 信封链路，MCP 侧各自处理，不在本次范围
- **file transfer 流特殊处理（修复 b6cf470）**：文件传输走二进制流而非 JSON 信封，bridge 侧 `sanitize_path`/`stat`/no-clobber 等失败时**主动发送错误信封**：`[0x02][msg_len: u16 LE][msg: UTF-8 bytes]`（1 字节状态码 0x02 + 2 字节小端长度 + 消息体）。MCP 侧 `upload_single`/`upload_dir`/`download` 读到 0x02 后读长度与消息并 `bail`，错误码经 `classify_error_message` 正确归类（如 PATH_TRAVERSAL）。触发点：上传 sanitize 失败、下载 sanitize 失败、下载 stat 失败、no-clobber 文件已存在、list stat 失败（bridge 侧 5 处）；消费点：上传单文件、上传目录、下载（MCP 侧 3 处）。

### 8.3 MCP 侧采用策略

`enrich_error()`（error.rs:131）：

- **新 bridge**：响应已带 error_code（bridge 注入）→ MCP 不覆盖 code，但**补齐缺失的 `recovery_hint` / `retryable`**（bridge 只注入 code，UX 字段由本层 `lookup()` 提供）——保证所有错误信封字段完整
- **旧 bridge**：响应无 error_code → MCP fallback `classify_message`（共享同一分类器，结果一致）
- `refused` 分支优先于 code 补齐（MCP 侧 exec precheck 产生，不经过 bridge，无冲突）
- 双向协议兼容：旧 MCP 忽略新字段；旧 bridge 不带字段

### 8.4 实施清单（与 §3 合并）

| 步骤 | 内容 | 文件 |
|------|------|------|
| 1 | 新增共享分类器（常量 + classify + 测试） | `clum-core/src/error_code.rs`（新） |
| 2 | MCP 分类器委托 clum-core + hint/retryable 表 + 新增模式 | `clum-mcp/src/error.rs` |
| 3 | bridge 出口注入 error_code | `rmux-bridge/src/proxy.rs` |
| 4 | 信封语义修复（5 处 ok:true→false） | `batch.rs`、`deploy.rs` |
| 5 | 透传逻辑修正 | `common.rs` |
| 6 | 文档同步 | `docs/TOOLS.md` |
| 7 | 验证 | `just check && just test && just lint` |

> 原"后续演进"章节内容已并入本节，作为本次实施的最终方案。错误码契约见 §4 表：**19 个共享分类器常量**（`clum_core::error_code.rs`）+ 1 个 MCP 侧特例 `REFUSED_STATE`（`error.rs` 硬编码，不走分类器）。
