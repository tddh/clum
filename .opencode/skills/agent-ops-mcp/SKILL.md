---
name: agent-ops-mcp
description: "使用 agent-ops MCP 工具操作远程主机的规范流程"
---

# agent-ops MCP 使用指南

## What is agent-ops

agent-ops is a **remote Linux operations platform** — not a simple SSH tool. Understanding these core concepts is essential before using any tools.

### Core Concepts

| Concept | Description |
|---------|-------------|
| **No direct SSH** | You do NOT hold SSH keys. All operations go through a Bridge proxy (QUIC-encrypted) deployed on each target host. |
| **Persistent sessions** | Sessions run inside **rmux** (a terminal multiplexer like tmux). They **survive disconnects** — you can disconnect and reconnect to the same session. Long-running commands keep running in the background. |
| **Shared sessions** | The same session can be used by AI (via MCP) and humans (via CLI `connect`) simultaneously or in turns. After you run a command, a human can attach and see the results. |
| **Session ≠ one-shot connection** | A session has a name, supports create/destroy, and contains panes/windows. Each `exec` call runs a command inside an existing session's pane — it does NOT open a new connection. |
| **Multi-host registry** | All operable hosts are registered in `hosts.yaml`. Use `host_list` to see them. Operations are uniform across hosts — same tools, different `host` parameter. |
| **Don't clean up by default** | Do NOT call `kill_session` / `close_pane` / `close_window` unless explicitly asked. Sessions are shared resources — a human or another AI might be using them. |

### Tool Selection Principles

- **Host is in the registry (`host_list`)** → Prefer agent-ops tools (they provide audit, persistence, and security management).
- **Host is NOT in the registry** → Use SSH/SCP/rsync directly. agent-ops can't reach hosts it doesn't know about.
- **User explicitly asks for SSH** → Respect the user's choice. Even if agent-ops could do it, use SSH when the user says so.

### Your Role

You are an SRE engineer operating remote Linux hosts via agent-ops MCP tools. You do NOT SSH directly to hosts — you operate through a Bridge proxy that manages an rmux terminal multiplexer on each host.

## 强制规则

### 1. 默认会话名

**必须使用** `session_name="agent-ops"`，除非用户明确说"创建新会话"或指定其他名称。

- ❌ 禁止自作主张创建 `test-session`、`debug-session` 等
- ✅ 始终使用 `agent-ops` 作为默认会话

### 2. 默认 Pane

**pane_id 可省略**。大多数工具（exec、send_keys、capture_pane 等）的 `pane_id` 参数是可选的：

- ✅ 省略 `pane_id` → server 自动选择 window 0 中编号最小的 pane
- ✅ 指定 `pane_id` → 使用指定的 pane
- ✅ 响应中会返回 `resolved_pane_id` 和 `auto_resolved: true`（自动探测时）
- ⚠️ **破坏性工具**（`close_pane`、`paste_buffer`、`respawn_pane`）**必须显式指定 pane_id**

### 3. 操作流程

```
session_attach(host, session_name="agent-ops")
→ 如果不存在：session_create(host, session_name="agent-ops")
→ exec(host, session_name, command="ls")  // pane_id 可省略
```

**示例**：
```json
// 省略 pane_id 的响应
{"ok":true,"output":"...","resolved_pane_id":"%0","auto_resolved":true}

// 指定 pane_id 的响应
{"ok":true,"output":"...","resolved_pane_id":"%3"}
```

### 4. 禁止行为

- ❌ 未经用户同意创建新会话
- ❌ 使用非 `agent-ops` 会话名（除非用户指定）
- ❌ 执行完命令后主动清理 session
- ❌ 对 `close_pane`/`paste_buffer`/`respawn_pane` 省略 pane_id

### 5. 会话生命周期

- ✅ **默认保留会话**：执行完命令后，不要主动关闭/清理 session
- ❌ 禁止调用 `kill_session`、`close_window`、`close_pane`（除非用户明确说"清理"、"关闭"、"销毁"）
- ✅ 用户可能需要查看执行结果或继续操作，保留会话是安全的默认行为

### 6. 以用户指令为主

用户的明确指令优先于以上所有默认规则。如果用户指令信息不明确，**必须先确认再执行**，禁止猜测。

**需要确认的场景：**
- ❓ 用户未指定主机 → "你要在哪台主机上操作？"
- ❓ 用户未指定操作目标 → "你要操作哪个文件/目录？"
- ❓ 用户说"清理一下"但未指定范围 → "你要清理哪些内容？"
- ❓ 用户指令有歧义 → 列出理解，让用户选择

**不需要确认的场景：**
- ✅ 用户明确说了主机名、会话名、命令等完整信息
- ✅ 上下文中已经明确

## 🔴 安全规则

### paste_buffer 是危险操作

`paste_buffer` 将粘贴板内容原样注入到目标 pane。**如果 pane 运行着 bash shell，bash 会逐行解释执行粘贴的每一行内容。** 这不是 bug——这是终端模拟粘贴的标准行为——但后果可能是灾难性的：

```
# buffer 内容（看起来无害）：
=== PING ===
PING www.a.shifen.com (110.242.69.21) 56(84) bytes of data.
HTTP/1.1 200 OK

# bash 逐行执行：
===: command not found          # → 报错，无害
-bash: syntax error ... '('     # → 报错，无害  
HTTP/1.1: No such file ...      # → 报错，无害

# 但如果 buffer 里是：
rm -rf /tmp/*
systemctl stop nginx
DROP TABLE users;
# → 真的会执行！
```

**强制规则：**

| 规则 | 说明 |
|------|------|
| **先查后贴** | 用 `list_buffers` 查看 buffer 内容（`preview` 字段）后再决定是否粘贴 |
| **禁止盲贴** | 绝不把未知/未检查的 buffer 内容粘贴到生产 shell |

## 工具使用示例

### ✅ 正确示例

```
# 1. 检查会话是否存在
session_attach(host="tf01", session_name="agent-ops")

# 2. 如果不存在，创建会话
session_create(host="tf01", session_name="agent-ops")

# 3. 直接执行命令（pane_id 可省略，自动探测）
exec(host="tf01", session_name="agent-ops", command="ls -la")

# 4. 也可以指定 pane_id
exec(host="tf01", session_name="agent-ops", pane_id="%3", command="ls -la")
```

### ❌ 错误示例

```
# 错误 1：自作主张创建新会话
session_create(host="tf01", session_name="test-session")  # ❌ 违反规则

# 错误 2：使用错误的会话名
exec(host="tf01", session_name="test-session", command="ls")  # ❌ 违反规则

# 错误 3：破坏性工具省略 pane_id
close_pane(host="tf01", session_name="agent-ops")  # ❌ 必须指定 pane_id

# 错误 4：执行完主动清理
close_pane(host="tf01", session_name="agent-ops", pane_id="%0")  # ❌ 违反规则
```

## 常用工具速查

| 场景 | 工具 |
|------|------|
| 跑命令看结果 | `exec` |
| 交互式程序 | `send_keys` + `capture_pane` |
| 等命令完成 | `wait_for_text` |
| 等进程退出 | `wait_exit` |
| 查看信息 | `pane_info` / `window_info` / `list_window_panes` |
| 多窗格分屏 | `split_pane` + `exec` |
| 分屏并执行命令 | `split_pane_with`（一步完成分屏+启动命令） |
| 特殊按键 | `send_keys`（`\x03`=Ctrl-C, `\n`=Enter） |
| 搜索 | `find_pane_text` / `find_text_all` |
| 大输出命令 | `collect_until_exit` |
| 长命令实时监控 | `stream_pane`（阻塞读，增量返回，替代 capture_pane 轮询） |
| 等终端稳定 | `wait_stable`（发送命令后待渲染完成再 capture） |
| 等特定字节序列 | `wait_for_bytes`（匹配 ANSI 序列等原始字节） |
| 区域截图 | `capture_region`（截取屏幕特定区域） |
| 多机并发 | `batch_exec` / `batch_upload` / `batch_download` |
| 端口转发 | `tunnel_create` / `tunnel_list` / `tunnel_close` |
| 文件传输 | `file_upload` / `file_download` |
| 批量文件传输 | `batch_upload` / `batch_download` |
| 部署 bridge | `deploy_bridge`（升级部署，需已运行 bridge） |
| 查询主机能力 | `host_capabilities`（检查 rmux 特性支持） |
| 查询 bridge 审计 | `query_bridge_audit`（查询目标主机 bridge 侧事件日志） |
| 列出录制文件 | `list_recordings`（列出已同步到本地的 PTY 录制） |
| 获取录制内容 | `get_recording`（获取 .cast 文件内容，访问被审计） |

## 终端状态感知（terminal_state）

以下 5 个工具的返回值中包含 `terminal_state` 和 `cursor` 字段：

| 工具 | terminal_state | cursor |
|------|:---:|:---:|
| `capture_pane` | ✅ | ✅ |
| `exec` | ✅ | ✅ |
| `wait_for_text` | ✅（成功时） | ✅（成功时） |
| `wait_stable` | ✅ | ✅ |
| `pane_info` | ✅ | ✅ |

### terminal_state 值含义

| 值 | 含义 | AI 应采取的动作 |
|---|------|----------------|
| `ready` | Shell 提示符，可以发送命令 | 正常发送命令 |
| `running` | 命令正在执行中 | 等待完成（`wait_stable` / `wait_for_text`） |
| `password` | 等待密码输入 | 提示用户输入密码，或发送密码 |
| `confirm` | 等待确认（[y/n]） | 发送 `y` 或 `n` |
| `repl` | 交互式环境（Python >>>、mysql>） | 发送 REPL 命令 |
| `editor` | 编辑器（vim、nano） | 发送编辑器按键，或 `\x1b:q!\n` 退出 |
| `pager` | 分页器（less、more） | 发送 `q` 退出 |
| `unknown` | 无法判断 | 用 `capture_pane` 查看文本自行判断 |

### 使用示例

```
# exec 返回 terminal_state，可以直接判断命令执行后的终端状态
exec(host, session_name, pane_id, command="vim file.txt")
→ {"ok": false, "terminal_state": "editor", ...}
→ 知道 vim 已打开，需要发送 \x1b:q!\n 退出

# capture_pane 返回 terminal_state，可以判断当前终端在干什么
capture_pane(host, session_name, pane_id)
→ {"terminal_state": "password", ...}
→ 知道终端在等密码输入，不应发送普通命令

# wait_stable 返回 terminal_state，可以判断命令完成后终端状态
send_keys("python3\n")
wait_stable(host, session_name, pane_id)
→ {"terminal_state": "repl", ...}
→ 知道已进入 Python REPL，可以发送 Python 代码
```

## exec 安全检查

`exec` 工具在执行命令前会检测终端状态。如果终端不在 `ready` 状态，exec 会拒绝执行并返回 `refused: true` + `error_code: "REFUSED_STATE"`。

### 为什么需要安全检查

| 场景 | 不检查的后果 |
|------|------------|
| 终端在 vim 中 | 命令被注入到编辑器，文件损坏 |
| 终端在 less 中 | 命令被当作搜索/导航输入 |
| 终端在等密码 | 命令被当作密码输入 |
| 终端在 REPL 中 | 命令被当作 Python/MySQL 代码执行 |

### 当 exec 返回 refused 时的决策框架

**核心原则：AI 决策，不是工具决策。** 工具只负责检测和拒绝，AI 根据上下文决定下一步。

1. 检查 `pre_terminal_state`，理解终端当前状态
2. 回溯对话历史：是你自己把终端带到这个状态的吗？
   - **是** → 你知道怎么退出，先退出再重试
   - **不是** → 用 `capture_pane` 查看终端内容，判断情况
3. 绝不在不理解终端状态的情况下强制发送按键

### 常见恢复模式

| pre_terminal_state | 恢复操作 |
|---|---|
| `editor` | `send_keys("\x1b:q!\n")` 退出 vim，或 `send_keys("\x18\x13")` 退出 nano |
| `pager` | `send_keys("q")` 退出 less/more |
| `password` | 提示用户输入密码，或 `send_keys("\x03")` 取消 |
| `confirm` | `send_keys("y\n")` 或 `send_keys("n\n")` |
| `running` | `wait_stable` 等待完成，或 `send_keys("\x03")` 中断 |
| `repl` | `send_keys("exit()\n")` 退出 REPL |
| `unknown` | `capture_pane` 查看文本后自行判断 |

## 常见场景

### 执行单个命令
```
1. list_window_panes(host, session_name, window_index=0) → 确认 pane_id
2. exec(host, session_name, pane_id, command) → 执行命令
```

### 交互式程序（vim, top, htop）
```
1. list_window_panes → 确认 pane_id
2. send_keys("vim file.txt\n") → 启动程序
3. capture_pane → 查看输出
4. send_keys("\x03") → Ctrl-C 退出
```

### 批量操作多台主机
```
batch_exec(hosts=["tf01", "dns-backup"], command="hostname")
结果以 hostname 为 key，直接取值
```

### 文件传输
```
单台：file_upload / file_download
批量：batch_upload / batch_download
```

### 等待长命令完成
```
1. send_keys("long-command\n")
2. wait_for_text(text="expected-output", timeout_ms=60000)
3. capture_pane → 获取结果
```

> ⚠️ **exec 超时不杀进程**：exec 的 timeout 只是客户端的等待上限，命令仍在远端 rmux pane 中运行。超时后可以用 `capture_pane` 查看进度，`wait_for_text` 等完成标志，或 `send_keys("\x03")` 中断。不要因为超时就重跑。
>
> ⚠️ **collect_until_exit 超时不同**：collect_until_exit 超时后**收集被取消（已收集的字节丢失）**，但远端进程**继续运行**。用 `capture_pane` 查看进度或 `wait_for_text` 等完成标志。不要用于 fire-and-forget 场景。

### 实时监控长命令输出（stream_pane）
```
# 适用于编译、下载等长时间运行的命令
1. send_keys("make build\n")
2. 循环调用 stream_pane(timeout_ms=5000) 直到完成
   - 首次调用返回当前快照 + 后续输出
   - 后续调用只返回新增输出
   - 比 capture_pane 轮询更高效
```

### 等待终端渲染完成（wait_stable）
```
# 适用于命令输出有动画或渐进式渲染的场景
1. send_keys("command\n")
2. wait_stable(stable_ms=500, timeout_ms=30000)
   - 等待输出 500ms 内无变化
3. capture_pane → 获取完整输出
```

### 端口转发访问远程服务
```
# 访问远程数据库
1. tunnel_create(host="tf01", local_port=15432, remote_host="127.0.0.1", remote_port=5432)
   → 返回 tunnel_id
2. 本地连接 localhost:15432 即可访问远程 PostgreSQL
3. tunnel_close(tunnel_id) → 关闭隧道

# 注意：如果主机配置了 allowed_tunnel_targets 白名单，
# 只有匹配的目标才允许创建隧道，不匹配会返回错误
```

### 分屏并执行命令（split_pane_with）
```
# 一步完成分屏 + 启动命令
split_pane_with(
  host="tf01",
  session_name="agent-ops",
  pane_id="%0",
  direction="vertical",
  command="tail -f /var/log/syslog",
  title="log-monitor"
)
→ 返回新 pane_id，命令已在新 pane 中启动
```

### 收集大输出命令结果（collect_until_exit）
```
# 适合大输出命令
# ⚠️ 超时后收集被取消（已收集字节丢失），但远端进程继续运行。不要用于 fire-and-forget 长任务
1. spawn_command(host, session_name, pane_id, command="find / -name '*.log'")
2. collect_until_exit(host, session_name, pane_id, max_bytes=10485760)
   → 流式收集所有输出直到进程退出
   → 返回收集的字节和退出信息
```

### 截取屏幕特定区域（capture_region）
```
# 截取状态栏或特定 UI 元素
capture_region(
  host="tf01",
  session_name="agent-ops",
  pane_id="%0",
  row=0, col=0, rows=1, cols=80  # 截取第一行
)
→ 返回指定区域的文本
```

### 查询主机能力
```
# 检查主机是否支持特定特性
host_capabilities(host="tf01", check="stream.control")
→ 返回 ok=true/false
```

## 错误处理

工具调用失败时返回结构化信封（MCP `isError: true`）：

```json
{
  "ok": false,
  "error": "pane id %99 was not found",
  "error_code": "PANE_NOT_FOUND",
  "recovery_hint": "list_window_panes 确认当前 pane_id（pane 可能已关闭）",
  "retryable": false
}
```

**处理规则**：
1. **优先按 `error_code` 分支**，不要匹配 `error` 字符串（措辞可能变，码是稳定契约）
2. **`recovery_hint` 就是下一步动作**，按它执行即可，无需查本表
3. **`retryable: false` 的错误禁止盲目重试**（如 TIMEOUT——命令可能还在远端运行）；`true`（网络类）可等待后重试

| error_code | 典型 `error` 消息 | 原因 | 解决方案 |
|-----------|------|------|---------|
| `PANE_NOT_FOUND` | `pane id %X was not found` | pane_id 错误或 pane 已关闭 | `list_window_panes` 确认当前 pane_id |
| `SESSION_NOT_FOUND` | `session not found` | 会话不存在 | `session_create` 创建会话 |
| `BRIDGE_UNREACHABLE` | `connection refused` | bridge 未运行 | 检查 `systemctl status rmux-bridge` |
| `TIMEOUT`（连接类） | `TCP connect timeout` | 主机离线或网络不通 | 确认主机在线、bridge 端口可达 |
| `AUTH_FAILED` | `authentication failed` | token 不匹配 | 检查 `hosts.yaml` 中的 `bridge_token` |
| `CONNECTION_LOST` | `recv: connection lost` | bridge 重启或网络中断 | 等待后重试 |
| `PANE_BUSY` | `pane still active` | spawn/shell_command 时 pane 非空闲 | 先 `close_pane` 或换 pane |
| `TIMEOUT`（执行类） | `timeout waiting for sentinel...` | 命令执行超时 | exec: 增大 `timeout_ms` 或检查命令是否卡住（⚠️ 超时后命令仍在运行！别重跑，用 capture_pane 补捞）。collect_until_exit: 超时后收集被取消（已收集字节丢失），但远端进程继续运行，用 capture_pane 或 wait_for_text 继续跟进。 |
| `PATH_TRAVERSAL` | `path traversal rejected` | 路径包含 `..` | 使用不含 `..` 的绝对路径或相对路径 |
| `TUNNEL_DENIED` | `tunnel target not in allowed list` | 隧道目标不在白名单中 | 检查 `hosts.yaml` 中的 `allowed_tunnel_targets` 配置 |
| `HOST_NOT_FOUND` | `host not found` | 主机名不在 registry 中 | `host_list` 检查可用主机 |
| `REFUSED_STATE` | （exec 安全拒绝，附具体建议） | 终端非 ready 状态 | 按 `error` 中的建议恢复终端状态后重试 |
| `INVALID_PARAMS` | `missing 'pane_id'` 等 | 缺少必填参数 | 对照该工具的 inputSchema.required 补全 |
| `SESSION_EXISTS` | `session already exists` | 同名会话已存在 | 直接 `session_attach` 或换个名称 |
| `WINDOW_NOT_FOUND` | `window not found` | 窗口不存在 | `window_info` / `select_window` 确认窗口 |
| `TUNNEL_NOT_FOUND` | `tunnel not found` | 隧道 ID 不存在 | `tunnel_list` 确认隧道 ID |
| — | 修改 `hosts.yaml` 后主机不生效 | 未重载配置 | 调用 `reload_config` 工具或 `kill -HUP <pid>` |

## 最佳实践

### 性能优化
- **避免 capture_pane 轮询**：使用 `stream_pane` 或 `wait_for_text` 替代
- **批量操作**：多台主机用 `batch_exec` 而非循环调用 `exec`
- **大输出命令**：用 `collect_until_exit`（流式收集，比逐次 capture 更高效）
- **合并诊断命令**：单主机多个只读诊断命令用 `&&` 合并为一个 `exec`（如 `df -h && free -m && uptime`），减少 LLM 推理轮次。可能触发 pager 的命令加 `--no-pager` 或 `| cat`
- **并发控制**：`batch_*` 工具支持 `concurrency` 参数，默认 5

### 安全实践
- **先查后贴**：`paste_buffer` 前必须 `list_buffers` 检查内容
- **危险操作需确认**：`close_pane`、`close_window`、`kill_session` 需用户明确同意
- **保留会话**：默认不清理会话，用户可能需要查看结果或继续操作
- **验证 pane_id**：每次操作前通过 `list_window_panes` 确认 pane_id
- **路径安全**：`file_upload`/`file_download` 的路径不能包含 `..`（bridge 端会拒绝路径穿越）
- **隧道白名单**：`tunnel_create` 受 `allowed_tunnel_targets` 配置限制，不匹配的目标会被拒绝

### 错误恢复
- **连接失败**：等待几秒后重试，可能是临时网络问题
- **Pane 非空闲**：使用 `respawn_pane(kill=true)` 强制重启，或换用其他 pane
- **命令超时**：增大 `timeout_ms`，或检查命令是否等待输入
- **会话丢失**：使用 `session_create` 重建会话

## 高级工具详解

### stream_pane - 实时流式输出
```
# 首次调用：建立连接，返回当前快照
stream_pane(host, session_name, pane_id, timeout_ms=5000)

# 后续调用：复用连接，只返回新增输出
stream_pane(host, session_name, pane_id, timeout_ms=5000)

# 适用场景：
# - 编译过程监控
# - 下载进度跟踪
# - 长命令实时输出
```

### tunnel_create - 端口转发
```
# 访问远程数据库
tunnel_create(
  host="tf01",
  local_port=15432,
  remote_host="127.0.0.1",  # 远程主机上的地址
  remote_port=5432
)
# → 本地连接 localhost:15432 即可访问远程 PostgreSQL

# 访问远程 Web 服务
tunnel_create(
  host="tf01",
  local_port=8080,
  remote_host="api.internal",
  remote_port=8080
)

# 安全限制：
# - 如果 hosts.yaml 配置了 allowed_tunnel_targets，
#   只有匹配 glob 模式的目标才允许
# - 例：allowed_tunnel_targets: ["127.0.0.1:5432", "10.0.1.*:*"]
# - 不配置 = 全部允许（向后兼容）
```

### wait_for_bytes - 等待原始字节
```
# 等待 ANSI 转义序列（如光标移动）
wait_for_bytes(
  host, session_name, pane_id,
  bytes="G1sK"  # base64 编码的原始字节
)

# 与 wait_for_text 的区别：
# - wait_for_text：匹配可见文本（ANSI 处理后）
# - wait_for_bytes：匹配原始字节流（包括 ANSI 序列）
```

### capture_region - 区域截图
```
# 截取状态栏
capture_region(host, session_name, pane_id, row=24, col=0, rows=1, cols=80)

# 截取特定 UI 元素
capture_region(host, session_name, pane_id, row=5, col=10, rows=3, cols=20)

# 全屏截图（省略坐标）
capture_region(host, session_name, pane_id)
```

## 工具选择

跑命令？
├── 会自行退出（ls, cat, grep）→ `exec`
├── 长程任务（ansible-playbook, terraform, 编译）→ `shell_command` + `wait_for_text` / `stream_pane`
├── 不会退出（tail -f, ping）→ `send_keys` + `capture_pane`
├── 大输出命令（find, du）→ `spawn_command` + `collect_until_exit`
│   ⚠️ 超时后收集被取消，远端进程继续运行
├── 需要实时监控输出 → `send_keys` + `stream_pane` 循环
├── 多台主机 → `batch_exec`
└── 需要分屏并行 → `split_pane_with`

需要输出？
├── 立即获取 → `capture_pane`
├── 等待特定文本 → `wait_for_text`
├── 等待进程退出 → `wait_exit`
├── 等待终端稳定 → `wait_stable`
├── 等待特定字节序列 → `wait_for_bytes`
├── 搜索文本 → `find_pane_text` / `find_text_all`
└── 截取特定区域 → `capture_region`

文件操作？
├── 单台上传 → `file_upload`
├── 单台下载 → `file_download`
├── 批量上传 → `batch_upload`
└── 批量下载 → `batch_download`

端口转发？
├── 创建隧道 → `tunnel_create`
├── 查看隧道 → `tunnel_list`
└── 关闭隧道 → `tunnel_close`

Pane 管理？
├── 分屏 → `split_pane`
├── 分屏并执行 → `split_pane_with`
├── 关闭 pane → `close_pane`（⚠️ 需用户明确同意）
├── 重启进程 → `respawn_pane`
├── 移动 pane → `break_pane` / `join_pane`
└── 交换 pane → `swap_pane`

Window 管理？
├── 新建窗口 → `split_window`
├── 关闭窗口 → `close_window`（⚠️ 需用户明确同意）
├── 切换窗口 → `select_window`
├── 调整布局 → `select_layout`
└── 重命名 → `rename_window`

## 违反后果

违反以上规则 = BUG，必须立即修正。
