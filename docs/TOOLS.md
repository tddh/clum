# yunying MCP 工具文档

> yunying 是一个 MCP Server，使 AI Agent 能够通过 RMUX SDK 远程控制 Linux 主机的交互式终端会话。所有工具通过 `host` 参数路由到目标主机。

## 约定

- `host` — 主机名，对应 `config/hosts.yaml` 中的 `name` 字段
- `session_name` — 会话名（如 `s1`、`agent`）
- `pane_id` — 窗格 ID（如 `%0`、`%4`）。大多数工具中可选，省略时自动探测 window 0 中编号最小的 pane。`close_pane`、`paste_buffer`、`respawn_pane` 必须显式指定。
- `window_index` — 窗口索引，从 0 开始
- `timeout_ms` — 超时毫秒数，默认 30000（`exec` / `batch_exec` 为 600000）
- 返回值统一为 JSON：`{"ok": true/false, ...}`

### 错误返回结构

业务失败（主机不存在、pane 不存在、超时、安全拒绝等）统一返回 result 信封，并标记 MCP `isError: true`：

```json
{
  "ok": false,
  "error": "pane id %99 was not found",
  "error_code": "PANE_NOT_FOUND",
  "recovery_hint": "list_window_panes 确认当前 pane_id（pane 可能已关闭）",
  "retryable": false
}
```

| 字段 | 说明 |
|------|------|
| `error` | 原始错误字符串（人读） |
| `error_code` | 稳定错误码（Agent 分支用），见下表 |
| `recovery_hint` | 建议的恢复动作 |
| `retryable` | 是否可安全重试同一调用 |

| error_code | 含义 |
|-----------|------|
| `HOST_NOT_FOUND` | 主机名不在注册表，用 `host_list` 核对 |
| `INVALID_PARAMS` | 缺少必填参数，对照该工具的 inputSchema |
| `SESSION_NOT_FOUND` / `SESSION_EXISTS` | 会话不存在 / 已存在 |
| `PANE_NOT_FOUND` / `PANE_BUSY` | pane 不存在 / 非空闲 |
| `WINDOW_NOT_FOUND` / `TUNNEL_NOT_FOUND` | 窗口 / 隧道不存在 |
| `PATH_TRAVERSAL` | 路径含 `..` 被拒绝 |
| `TUNNEL_DENIED` | 隧道目标不在 `allowed_tunnel_targets` 白名单 |
| `AUTH_FAILED` | bridge token 不匹配 |
| `BRIDGE_UNREACHABLE` / `CONNECTION_LOST` | bridge 未运行 / 连接中断（可重试） |
| `TIMEOUT` | 超时（exec 超时不杀进程，先 capture_pane 看进度再决定，勿盲目重跑） |
| `REFUSED_STATE` | exec 安全检查拒绝（终端非 ready），`error` 含具体恢复建议 |
| `UNKNOWN` | 未分类错误，看 `error` 详情 |

未知工具名按 MCP 规范返回 JSON-RPC `-32602`；未知方法返回 `-32601`。

---

## 主机管理

### `host_list`

列出所有主机（enrolled 优先）。Enrolled 主机来自 bridge 注册（`via: "enrolled"`, `online: true`），直连主机来自 hosts.yaml（`via: "direct"`, `online: null`）。同一主机两边都有时，enrolled 元数据优先。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| 无 | | |

**返回** `{"hosts": [...], "count": N}`

### `host_filter`

按条件过滤主机。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `group` | string | | 按分组过滤 |
| `tags` | string[] | | 同时匹配所有 tag |
| `label_key` | string | | 标签键 |
| `label_value` | string | | 标签值 |
| `pattern` | string | | 主机名 glob 模式，如 `prod-web-*` |

### `host_set_meta`

设置 enrolled 主机的元数据（group/tags/labels），持久化到 SQLite，立即生效于 `host_list`/`host_filter`。仅对通过 `bridge add` 注册的主机有效。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名 |
| `group` | string | | 分组名，如 production, infra |
| `tags` | string[] | | 替换 tags 列表 |
| `labels` | object | | 键值对标签，如 `{"role": "mcp-server"}` |

---

### `reload_config`

重新加载 `hosts.yaml` 配置，无需重启 MCP Server。修改配置文件（增删改主机）后调用此工具即可生效。加载失败时保留原有配置不变。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| 无 | | |

**返回** `{"ok": true, "hosts_count": 3, "message": "successfully reloaded 3 hosts"}`

失败时返回 `{"ok": false, "error": "failed to parse hosts YAML: ..."}` — 此时运行中服务不受影响。

> **两种触发方式**：除了通过此 MCP 工具触发，运维人员也可以通过 `kill -HUP <pid>` 向 MCP Server 进程发送 SIGHUP 信号来触发配置重载。

---

## 会话管理

### `session_create`

在指定主机上创建新的终端会话。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名称 |
| `session_name` | string | | 会话名称（可选，默认 `yunying`） |

**返回** `{"ok": true, "session_name": "...", "pane_id": "%N"}`

### `session_list`

列出指定主机上的所有活动会话。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |

**返回** `{"ok": true, "sessions": [{"session_name": "..."}]}`

### `session_attach`

检查会话是否存在。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |

> **注意**：当前仅检查存在性，不执行真正的 attach。

### `session_detach`

检查会话是否存在。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |

> **注意**：当前仅检查存在性，不执行真正的 detach。

---

## 终端输入

### `send_keys`

向窗格发送按键。用于特殊键（Ctrl-C=`\x03`、Enter=`\n`、Tab=`\t`、Escape=`\x1b` 等）。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | | 可选，省略时自动探测 |
| `keys` | string | ✅ |

### `send_text`

向窗格发送字面文本。与 `send_keys` 区别：`send_text` 发送纯文本，`send_keys` 额外支持特殊键 token。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | | 可选，省略时自动探测 |
| `text` | string | ✅ |

---

## 终端输出

### `capture_pane`

捕获窗格文本（默认最后 200 行，`max_lines=0` 返回全部 scrollback）。支持高级选项：ANSI 保留、行范围截取、换行拼接、空格保留、交替屏捕获、写入 buffer。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | | 可选，省略时自动探测 |
| `max_lines` | integer | | 默认 200，0=不限制 |
| `ansi` | boolean | | 保留 ANSI 转义码（默认 false），true 时 text 为 base64 |
| `start_line` | integer | | 起始行（负数 = 从末尾算），覆盖 max_lines |
| `end_line` | integer | | 结束行（负数 = 从末尾算） |
| `join_wrapped` | boolean | | 拼接终端自动换行的行（默认 false） |
| `preserve_spaces` | boolean | | 保留尾部空格（默认 false） |
| `alternate` | boolean | | 捕获交替屏（如 vim/less），默认 false |
| `buffer_name` | string | | 写入指定 buffer 而非返回文本 |

**返回** `{"ok": true, "text": "...", "terminal_state": "ready", "cursor": {"row": 0, "col": 14, "visible": true}}`

> `terminal_state` 为终端状态分类：`ready`（shell 提示符）、`running`（命令执行中）、`password`（等待密码）、`confirm`（等待确认）、`repl`（交互式环境）、`editor`（编辑器）、`pager`（分页器）、`unknown`。

### `wait_for_text`

等待窗格中出现指定文本（带超时）。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | | 可选，省略时自动探测 |
| `text` | string | ✅ | 等待出现的文本 |
| `timeout_ms` | number | | 等待超时毫秒数，默认 30000 |

**返回** `{"ok": true, "found": true, "terminal_state": "ready", "cursor": {"row": 0, "col": 14, "visible": true}}`

超时：`{"ok": false, "found": false, "error": "timeout waiting for: ..."}`

### `stream_pane`

**阻塞读取窗格输出流**。首次调用自动创建流（返回当前快照 + 后续增量输出），后续调用复用同一流（只返回新增内容）。阻塞直到有新数据或超时。适用于长命令（编译、日志监控）的实时输出跟踪，替代 capture_pane 轮询。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | | 可选，省略时自动探测 |
| `timeout_ms` | number | ❌ (默认 10000) |

**返回** `{"text": "新增输出内容..."}` 或 `{"text": ""}`（超时无数据或流断开）

### `find_pane_text`

在窗格可见文本中搜索，返回第一个匹配的位置。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | | 可选，省略时自动探测 |
| `pattern` | string | ✅ |

**返回** `{"ok": true, "found": true, "match": {"start_row": 0, "start_col": 0, "end_row": 0, "end_col": 4, "text": "root"}}`

---

### `find_text_all`

搜索可见文本中**所有**匹配（包括同一行上的重叠匹配）。`find_pane_text` 只返回第一个，此工具返回全部。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | | 可选，省略时自动探测 |
| `pattern` | string | ✅ |

**返回** `{"ok": true, "matches": [{"start_row": 5, "start_col": 0, "end_row": 5, "end_col": 5, "text": "ERROR"}], "count": 2}`

---

### `wait_for_bytes`

等待原始字节流中出现指定模式。比 `wait_for_text` 更底层，可匹配 ANSI 序列和控制字符。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | | 可选，省略时自动探测 |
| `bytes` | string | ✅ | base64 编码的目标字节串 |
| `only_new` | boolean | | 仅匹配新数据（跳过历史），默认 false |
| `timeout_ms` | number | | ⚠️ 当前 bridge 侧未强制执行，实际为无限等待。不要依赖此超时 |

**返回** `{"ok": true, "found": true}`

超时（当 bridge 侧实现后）：`{"ok": false, "found": false, "error": "..."}`

---

## 发现与查询

跨 session/pane 的发现和过滤。

### `find_panes`

跨 session 按标题、命令、工作目录或进程状态发现 pane。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名 |
| `session_name` | string | | 按 session 过滤 |
| `title` | string | | 标题精确匹配 |
| `title_prefix` | string | | 标题前缀匹配 |
| `command_contains` | string | | 命令包含此字符串 |
| `cwd_contains` | string | | 工作目录包含此字符串 |
| `window_index` | integer | | 按窗口索引过滤 |
| `running` | boolean | | 仅运行中 |
| `exited` | boolean | | 仅已退出 |

**返回** `{"ok": true, "panes": [{"pane_id": "%2", "session_name": "yunying", "window_index": 0, "title": "nginx-log", "command": [...], "working_directory": "/var/log", "process": "running", "pid": 12345}], "count": 1}`

### `find_sessions`

按名称发现 session。**与 `session_list` 的区别**：`session_list` 仅返回名称列表，`find_sessions` 返回 live handle，适合后续查询 pane 和窗口。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名 |
| `name` | string | | session 名称（省略返回全部） |

**返回** `{"ok": true, "sessions": [{"session_name": "yunying"}], "count": 1}`

### `get_pane_title`

获取指定 pane 的标题。配合 `set_pane_title` 使用。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | | 可选，省略时自动探测 |

**返回** `{"ok": true, "pane_id": "%0", "title": "nginx-log"}` — 无标题时 `"title": null`

### `get_pane_by_title`

按标题精确查找单个 pane（要求恰好 1 个匹配，0 或多个均报错）。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `title` | string | ✅ |

**返回** `{"ok": true, "found": true, "pane": {"pane_id": "%2", "session_name": "yunying", "title": "nginx-log", ...}}`

### `host_capabilities`

查询主机 daemon 支持的功能列表。在执行高级操作（如 web share 等）前验证兼容性。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名 |
| `check` | string | | 检查特定 capability（可选，如 `"sixel"`） |

**返回** `{"ok": true, "capabilities": ["web.share", "sdk.waits", ...], "count": 20}` — 指定 `check` 时额外返回 `"has_capability": true/false`

### `capture_region`

捕获 pane 矩形区域或全屏截图。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | | 可选，省略时自动探测 |
| `row` | integer | | 起始行（0-based） |
| `col` | integer | | 起始列（0-based） |
| `rows` | integer | | 高度（行数） |
| `cols` | integer | | 宽度（列数） |
| `styled` | boolean | | 保留样式标记，默认 false |

四个坐标参数要么全传（矩形区域），要么全不传（全屏截图）。部分传参报错。

**返回** `{"ok": true, "text": "...", "styled": false}`

---

## 命令执行

### `exec`

一站式命令执行。内部自动完成：安全检查 → 发送命令（注入 start/end 标记）→ 事件驱动等待结束标记 → 从 scrollback 渐进扩大窗口捕获 → 截取标记区间原文返回。

**输出格式**：`output` 为 start_marker → sentinel 区间内的**完整终端上下文**（含 shell 提示符、命令回显），不做行级过滤——所见即所得。大输出命令（数百/数千行）在 daemon history-limit（默认 50000 行）内完整捕获。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名称 |
| `session_name` | string | ✅ | 会话名称 |
| `pane_id` | string | | 可选，省略时自动探测 |
| `command` | string | ✅ | shell 命令 |
| `timeout_ms` | number | | 兜底超时毫秒数，默认 600000（10 分钟）。正常命令（含编译、apt/yum 安装、docker pull）无需设置——等待命令执行完毕是默认行为 |
| `max_lines` | integer | | 保留输出的最后 N 行（默认 200，0=不限制）。完整输出始终从 scrollback 捕获，此参数仅截断返回量 |
| `clear_screen` | boolean | | 执行前是否清屏，默认 false |

> **安全检查**：exec 执行前会检测终端状态。如果终端不在 `ready` 状态（如 vim、less、password prompt、REPL 等），exec 会拒绝执行并返回 `refused: true`。这是为了防止命令注入到非 shell 环境。

**新增响应字段**：
- `pre_terminal_state`：执行前检测到的终端状态（`ready` / `running` / `password` / `confirm` / `repl` / `editor` / `pager` / `unknown`）
- `refused`：仅在被安全检查拒绝时出现，值为 `true`

**成功返回** `{"ok": true, "output": "...", "exit_code": 0, "duration_ms": 70, "terminal_state": "ready", "pre_terminal_state": "ready", "cursor": {"row": 5, "col": 14, "visible": true}}`

**超时返回**：`{"ok": false, "output": "...", "exit_code": null, "error": "timeout...", "terminal_state": "running", "pre_terminal_state": "ready", "cursor": {"row": 5, "col": 0, "visible": true}}`

**安全检查拒绝**：
```json
{"ok": false, "refused": true, "error": "Terminal is in editor (vim/nano). Use send_keys to interact with editor, or exit editor first.", "pre_terminal_state": "editor"}
```

> `terminal_state`、`pre_terminal_state` 和 `cursor` 仅在 bridge 支持时返回（向后兼容）。

✅ 适用：一次性会自行退出的命令（`ls`、`cat`、`grep`、`systemctl`、`kubectl`、`apt-get`、`curl` 等）
❌ 不适用：交互式程序（`vim`、`htop`、`less`）、不自动退出的命令（`tail -f`、`nc -l`、`ping`）

> 对不自动退出的命令，请用 `send_keys` 发送，然后用 `wait_for_text`/`capture_pane` 观察输出，用 `send_keys("\x03")` 发送 Ctrl-C 中断。

> **断连自愈**：等待期间连接断开（bridge 重启、网络抖动、QUIC idle）时，exec 自动退避重连并继续等待——sentinel 标记存在于远端 pane 上，重连不影响命令执行，整个恢复过程计入同一时间预算。

> ⚠️ **超长时间命令**（如 `ansible-playbook`、`terraform apply`、超大型编译等可能超过 10 分钟的任务）：推荐用 `send_keys`/`shell_command` 启动 + `collect_until_exit`/`stream_pane` 收集结果，比 exec 更合适。`exec` 超时后**命令仍在远端运行**，session 保留，可以后期 `capture_pane` 查看进度、`wait_for_text("PLAY RECAP")` 等完成标志 |

### `wait_exit`

等待窗格中进程退出并返回退出状态。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | | 可选，省略时自动探测 |
| `timeout_ms` | number | | 超时毫秒数，默认 30000 |

**返回** `{"ok": true, "exited": true, "exit_code": 0, "signal": null}`

### `collect_until_exit`

收集 pane 从此刻到进程退出的全部输出。适合大输出量的命令。

> ⚠️ Pane 进程**必须先已运行**（通过 `send_keys` 或 `shell_command` 启动）。输出以 base64 编码返回。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | | 可选，省略时自动探测 |
| `max_bytes` | integer | | 最大收集字节数，默认 1048576 (1MB) |
| `timeout_ms` | number | | 超时毫秒数，默认 60000 |
| `starting_at` | string | | `"now"`（默认，从最新输出后开始）或 `"oldest"`（从保留的最旧输出开始） |

**返回** `{"ok": true, "output": "<base64>", "collected_bytes": 4096, "exit_code": 0, "signal": null, "truncated": false, "duration_ms": 1234}`

> ⚠️ **超时行为**：超时后**收集被取消（已收集的字节丢失）**，但远端进程**继续运行**（与 `exec` 类似，命令不会被杀）。可用 `capture_pane` 查看进度或 `wait_for_text` 等完成标志。如果你需要"fire and forget"式的长时间执行且保留输出，请用 `shell_command` + 后期 `capture_pane`/`wait_for_text`，而不是 `collect_until_exit`。

---

### `wait_stable`

等待 pane 输出稳定（无变化持续指定毫秒）。适合在 `exec` 或 `send_keys` 发送命令后，等终端渲染完成再 capture。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | | 可选，省略时自动探测 |
| `stable_ms` | number | | 稳定持续毫秒数，默认 500 |
| `timeout_ms` | number | | 最大等待毫秒数，默认 30000 |

**返回** `{"ok": true, "stable": true, "terminal_state": "ready", "cursor": {"row": 5, "col": 14, "visible": true}}`

超时：`{"ok": false, "stable": false, "error": "timeout: pane did not stabilize within ..."}`

---

## 窗格操作

### `split_pane`

在当前窗格内分屏，返回新 pane 的 ID。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | | 可选，省略时自动探测 |
| `direction` | string | | `vertical`（左右分屏）或 `horizontal`（上下分屏），默认 `horizontal` |

**返回** `{"ok": true, "pane_id": "%N"}` — 新 pane 的 ID

### `split_window`

在会话中创建新窗口（非 pane 分屏）。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `direction` | string | | `horizontal` 或 `vertical`（当前无效，仅兼容保留） |

> **注意**：`split_window` 创建全新空 window，不含额外 pane。如需 pane 级别的左右/上下分屏，请用 `split_pane`。

### `split_pane_with`

分屏并同时在新 pane 中启动命令（原子操作，避免先分屏再 spawn 的中间状态）。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | | 可选，省略时自动探测 | |
| `direction` | string | ✅ | `horizontal` 或 `vertical` |
| `command` | string | ✅ | 在新 pane 中运行的命令 |
| `args` | string[] | | 命令参数 |
| `shell` | boolean | | `true`=通过 `/bin/sh -c` 执行，默认 `true` |
| `cwd` | string | | 新 pane 的工作目录 |
| `env` | object | | 环境变量，`KEY:VALUE` 键值对 |
| `title` | string | | 新 pane 的标题 |
| `keep_alive_on_exit` | boolean | | 命令退出后保留 pane（默认 false） |

**返回** `{"ok": true, "new_pane_id": "%N"}`

### `resize_pane`

调整窗格尺寸（列数 × 行数）。

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|:---:|:---:|------|
| `host` | string | ✅ | | |
| `session_name` | string | ✅ | | |
| `pane_id` | string | | 可选，省略时自动探测 |
| `cols` | integer | | 80 | 列数（宽度） |
| `rows` | integer | | 24 | 行数（高度） |

### `set_pane_title`

设置窗格标题。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | | 可选，省略时自动探测 |
| `title` | string | ✅ |

### `clear_history`

清除 pane 滚动历史。与 `exec` 的 `clear_screen` 不同（后者仅清可见区域），此工具移除全部保留的输出。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | | 可选，省略时自动探测 |

### `close_pane`

关闭窗格（杀死 pane 进程）。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | ✅ | 窗格 ID，如 `%4` |

**返回** `{"ok": true, "closed": true}`

### `break_pane`

将 pane 从当前窗口分离为独立窗口（或移到指定窗口）。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | | 源 pane（省略则使用当前活跃 pane） |
| `destination_window` | integer | | 目标窗口索引（省略则创建新窗口） |
| `detached` | boolean | | 不切换焦点到新窗口（默认 false） |

### `join_pane`

将 pane 移动到另一个窗口，拆分到目标 pane 旁边。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `source_pane_id` | string | ✅ | 要移动的 pane |
| `target_pane_id` | string | ✅ | 目标 pane（源 pane 将放到它旁边） |
| `direction` | string | | `horizontal`（上下）或 `vertical`（左右），默认 vertical |
| `size` | integer | | 新 pane 尺寸（行数/列数），省略则均分 |

### `swap_pane`

交换两个 pane 在窗口布局中的位置。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `source_pane_id` | string | ✅ | |
| `target_pane_id` | string | ✅ | |
| `detached` | boolean | | 不改变焦点（默认 false） |

---

## 窗口操作

### `close_window`

关闭窗口（杀死其中所有 pane）。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `window_index` | integer | ✅ |

**返回** `{"ok": true, "closed": true}`

### `rename_window`

重命名窗口。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `window_index` | integer | ✅ |
| `name` | string | ✅ |

### `resize_window`

调整窗口尺寸。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `window_index` | integer | ✅ | |
| `width` | integer | | 宽度（可选） |
| `height` | integer | | 高度（可选） |

### `select_window`

将指定窗口设为活跃窗口。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `window_index` | integer | ✅ |

### `select_layout`

应用窗口布局。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `window_index` | integer | ✅ | |
| `layout` | string | ✅ | `even-horizontal` / `even-vertical` / `main-horizontal` / `main-vertical` / `tiled` |

---

## 信息查询

### `pane_exists`

检查窗格是否存在。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | | 可选，省略时自动探测 |

**返回** `{"ok": true, "exists": true}`

### `pane_info`

获取窗格详细信息。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_id` | string | | 可选，省略时自动探测 |

**返回** `{"ok": true, "info": {"pane_id": "%0", "window_id": "@0", "session_id": "$0", "index": 0, "size_cols": 170, "size_rows": 39, "command": null, "working_directory": "/root", "tags": []}, "terminal_state": "ready", "cursor": {"row": 0, "col": 14, "visible": true}}`

> `terminal_state` 和 `cursor` 为附加字段，snapshot 失败时值为 `null`。

### `window_info`

获取窗口详细信息（名称、尺寸、索引）。需列出窗格请用 `list_window_panes`。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `window_index` | integer | ✅ |

**返回** `{"ok": true, "info": {"window_id": "@0", "name": "ops", "size_cols": 170, "size_rows": 40, "index": 0}}`

### `list_window_panes`

列出窗口中所有窗格。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `window_index` | integer | ✅ |

**返回** `{"ok": true, "panes": [{"pane_id": "%0", "active": true}]}`

---

## 生命周期

### `kill_session`

销毁整个会话（所有 window/pane/进程）。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |

**返回** `{"ok": true, "killed": true}`

---

## 进程控制

### `shell_command`

通过 shell 执行命令（`/bin/sh -c`），替换当前 pane 的进程。Pane 必须空闲。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | | 可选，省略时自动探测 |
| `command` | string | ✅ | 要执行的 shell 命令 |

### `respawn_pane`

重新 spawn 窗格进程。用于 pane 中进程已退出或需要重置 shell 环境时。支持自定义命令、环境变量、工作目录等。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | ✅ | |
| `command` | string | | 替换默认 shell（可选） |
| `args` | string[] | | 命令参数（`shell=false` 时使用） |
| `shell` | boolean | | 通过 `/bin/sh -c` 执行（默认 false） |
| `cwd` | string | | 工作目录 |
| `env` | object | | 环境变量，`KEY:VALUE` 键值对 |
| `kill` | boolean | | 强制 kill 当前进程再 respawn（默认 false） |
| `keep_alive_on_exit` | boolean | | 进程退出后保留 pane（默认 false） |

---

## 高级编排

### `broadcast_keys`

向同一会话中的多个 pane 同时发送相同按键。支持特殊键（同 `send_keys`）。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `session_name` | string | ✅ |
| `pane_ids` | string[] | | 目标 pane ID 列表（省略则广播到所有 pane） |
| `keys` | string | ✅ |

### `cmd_escape`

直接调用 rmux 命令行工具。当 bridge 协议未覆盖某些操作时使用。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `args` | string[] | | rmux CLI 参数（如 `["list-sessions"]`） |

**返回** `{"ok": true, "stdout": "...", "stderr": "", "exit_code": 0}`

---

## 粘贴板操作

### `list_buffers`

列出所有粘贴板 buffer。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |

**返回** `{"ok": true, "buffers": [{"name": "buffer0", "size": 1024, "preview": "..."}], "count": 1}`

### `paste_buffer`

将 buffer 内容粘贴到指定 pane。

> ⚠️ 如果 pane 运行着 bash shell，bash 会逐行解释执行粘贴内容。务必先用 `list_buffers` 检查内容后再粘贴。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | |
| `session_name` | string | ✅ | |
| `pane_id` | string | ✅ | |
| `buffer_name` | string | | buffer 名称（省略则粘贴最近一个） |

### `delete_buffer`

删除指定 buffer。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| `host` | string | ✅ |
| `buffer_name` | string | ✅ |

---

## 文件传输

### `file_upload`

上传文件或目录到远程主机（通过 bridge QUIC 通道）。目标目录不存在时自动创建。支持覆盖策略和 glob 排除。Bridge 端会拒绝包含 `..` 的路径（防路径穿越攻击）。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名称 |
| `local_path` | string | ✅ | 本地文件路径 |
| `remote_path` | string | ✅ | 远程目标路径 |
| `overwrite` | string | | 覆盖策略: `overwrite`(默认) / `skip` / `rename` / `error` |
| `exclude` | string[] | | glob 排除模式，如 `["*.log", "target/**/*"]` |

**返回** `{"ok": true, "files": [...], "total": N, "uploaded": N, "skipped": N, "failed": 0}`

每个 file 对象：`{"path": "...", "status": "uploaded"|"skipped", "size": N, "sha256": "..."}`

> ⚠️ **AI Agent 使用约束**：除非用户明确指定，不要自作主张添加 `exclude` 或修改 `overwrite`。用户说传什么就传什么，不明白就二次确认。

### `file_download`

从远程主机下载文件或目录（通过 bridge QUIC 通道）。自动检测远程路径类型：单文件直接下载；目录则递归下载所有文件，保持目录结构。返回文件大小和 SHA256 校验值。Bridge 端会拒绝包含 `..` 的路径（防路径穿越攻击）；MCP 端会验证远端返回的相对路径不含 `..` 且非绝对路径。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `host` | string | ✅ | 主机名称 |
| `remote_path` | string | ✅ | 远程文件或目录路径 |
| `local_path` | string | ✅ | 本地保存路径（目录下载时为本地根目录） |

**单文件返回** `{"ok": true, "file": {"uri": "...", "local_path": "...", "size": N, "sha256": "..."}}`

**目录返回** `{"ok": true, "files": [...], "total": N}`

> ⚠️ **AI Agent 使用约束**：除非用户明确指定，不要添加额外过滤或修改路径。用户说下载什么就下载什么，不明白就二次确认。

---

## 操作审计

审计系统自动记录所有 MCP 工具调用到 SQLite 数据库，支持安全审计、运维排错、用量统计。

**数据库路径**：`~/.yunying/audit.db`（可通过 `--audit-db` 自定义）
**保留策略**：90 天 + 500MB 上限（先触发者生效）
**自动清理**：MCP Server 启动时后台每 10 分钟检查一次

审计数据通过 CLI 子命令查询，不依赖 MCP 协议：

### `audit query`

```bash
yunying-mcp audit query [--db <path>] [--host <host>] [--action <action>]
    [--agent <agent>] [--since <ISO8601>] [--until <ISO8601>]
    [--success <true|false>] [--limit <n>] [--format <table|json|jsonl>]
```

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `--db` | path | | 数据库路径，默认 `~/.yunying/audit.db` |
| `--host` | string | | 按主机名过滤 |
| `--action` | string | | 按操作类型过滤（Exec、FileUpload 等） |
| `--agent` | string | | 按 AI Agent 名称过滤 |
| `--since` | string | | 起始时间（ISO 8601） |
| `--until` | string | | 截止时间（ISO 8601） |
| `--success` | bool | | 按成功/失败过滤 |
| `--limit` | int | | 返回条数，默认 50 |
| `--format` | string | | 输出格式：`table`（默认）、`json`、`jsonl` |

### `audit stats`

```bash
yunying-mcp audit stats [--db <path>] [--since <ISO8601>]
```

输出总数、成功率、Top 主机/操作/Agent、平均耗时、最近失败。

### `audit cleanup`

```bash
yunying-mcp audit cleanup [--db <path>] [--older-than <days>] [--max-size <mb>]
```

手动触发清理。

### 审计事件字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `id` | int | 自增主键 |
| `event_id` | uuid | 事件唯一 ID |
| `timestamp` | ISO 8601 | 操作时间（UTC） |
| `agent_name` | string | AI Agent 名称（来自 MCP initialize） |
| `host_name` | string | 目标主机 |
| `session_name` | string | 会话名 |
| `pane_id` | string | 窗格 ID（非 pane 操作为空） |
| `action` | string | 操作类型（66 种 AuditAction） |
| `detail` | string | 操作参数 |
| `output_summary` | string | Exec/CmdEscape 的输出摘要（前 500 字符） |
| `success` | bool | 操作是否成功 |
| `duration_ms` | int | 操作耗时（毫秒） |
| `error_message` | string | 失败时的错误信息 |

---

## 快速参考

| 场景 | 工具 |
|------|------|
| 看主机列表 | `host_list` / `host_filter` |
| 重载主机配置 | `reload_config` |
| 跑命令看结果 | `exec` |
| 收集大输出 | `collect_until_exit`（比 exec 更高效） |
| 交互式程序 | `send_keys` + `capture_pane` |
| 等命令完成 | `wait_for_text` |
| 等进程退出 | `wait_exit` |
| 发现 pane | `find_panes` / `get_pane_by_title` |
| 发现 session | `find_sessions` |
| 查看信息 | `pane_info` / `window_info` / `list_window_panes` |
| 多窗格分屏 | `split_pane` / `split_pane_with` |
| 布局调整 | `break_pane` / `join_pane` / `swap_pane` |
| 粘贴板 | `list_buffers` / `paste_buffer` / `delete_buffer` |
| 清理 | `close_pane` → `close_window` → `kill_session` / `clear_history` |
| 特殊按键 | `send_keys`（`\x03`=Ctrl-C, `\n`=Enter） |
| 搜索 | `find_pane_text` / `find_text_all` / `wait_for_bytes` |
| 能力检测 | `host_capabilities` / `wait_stable` |
| 审计查询 | `yunying-mcp audit query --host tf01 --action exec --format table` |
| 审计统计 | `yunying-mcp audit stats` |
| 多机并发执行 | `batch_exec` |

---

## 批量操作

### `batch_exec`

Execute the same command on multiple hosts concurrently. Sends the command to all specified hosts in parallel, waits for each to complete via sentinel markers (event-driven detection), captures output per host, and returns results keyed by hostname. Host-level failures (connection refused, timeout) do not affect other hosts. Non-zero exit codes set per-host ok=false (but output is always captured — check per-host exit_code). For self-terminating commands only.

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `hosts` | string[] | ✅ | 主机名列表 |
| `command` | string | ✅ | 要在每台主机上执行的命令 |
| `timeout_ms` | number | | 每台主机超时毫秒数（默认 600000 = 10 分钟） |
| `max_lines` | integer | | 每台主机最大返回行数（默认 200，0=不限制） |
| `concurrency` | integer | | 最大并发连接数（默认 5，0=不限制） |

**返回**

```json
{
  "ok": true,
  "command": "uptime",
  "total": 3,
  "success": 2,
  "failed": 1,
  "total_duration_ms": 2345,
  "results": {
    "tf01": {
      "ok": true,
      "output": "12:34:56 up 30 days ...",
      "exit_code": 0,
      "duration_ms": 1234
    },
    "tf02": {
      "ok": false,
      "output": "",
      "exit_code": null,
      "duration_ms": 5000,
      "error": "connect: connection refused"
    }
  }
}
```

- `results` 以主机名为 key，直接取值不遍历
- 单台主机故障（连接失败/超时/命令错误）不抛异常，在对应 result 中标记 `ok: false` + `error`
- `total_duration_ms` 是墙钟时间（所有主机中最长的那台），反映并发效果
- 非零 exit_code 会导致对应主机的 `ok: false`，但输出始终会捕获——检查 per-host 的 `exit_code` 字段判断命令实际结果
- 内部通过 `yunying` session 的默认 pane `%0` 执行，行为与 `exec` 一致

### `batch_upload`

Upload a file or directory to multiple hosts concurrently.

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `hosts` | string[] | ✅ | 主机名列表 |
| `local_path` | string | ✅ | 本地文件或目录路径 |
| `remote_path` | string | ✅ | 远程目标路径 |
| `overwrite` | string | | overwrite\|skip\|rename\|error（默认 overwrite） |
| `exclude` | string[] | | 排除 glob 模式 |
| `concurrency` | integer | | 最大并发连接数（默认 5，0=不限制） |

**返回**

```json
{
  "ok": true,
  "total": 3,
  "success": 2,
  "failed": 1,
  "total_duration_ms": 5678,
  "results": {
    "tf01": {
      "ok": true,
      "files": [{"path": "/opt/app", "status": "uploaded", "size": 12345, "sha256": "abc..."}],
      "total": 1, "uploaded": 1, "skipped": 0, "failed_count": 0
    },
    "tf02": {
      "ok": false,
      "error": "connect: connection refused"
    }
  }
}
```

### `batch_download`

Download a file from multiple hosts concurrently. Each host's file is saved to `local_dir/<hostname>/<filename>`. ⚠️ Multiple runs to the same `local_dir` WILL overwrite previous downloads — use different `local_dir` per run to preserve history.

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `hosts` | string[] | ✅ | 主机名列表 |
| `remote_path` | string | ✅ | 远程文件路径 |
| `local_dir` | string | ✅ | 本地目录（自动创建 `<hostname>/` 子目录） |
| `concurrency` | integer | | 最大并发连接数（默认 5，0=不限制） |

**返回**

```json
{
  "ok": true,
  "total": 2,
  "success": 1,
  "failed": 1,
  "total_duration_ms": 3456,
  "results": {
    "tf01": {
      "ok": true,
      "file": {
        "remote_path": "/var/log/syslog",
        "local_path": "./logs/tf01/syslog",
        "size": 987654,
        "sha256": "def..."
      }
    },
    "tf02": {
      "ok": false,
      "error": "download failed: file not found"
    }
  }
}
```

---

## 端口转发

### `tunnel_create`

创建本地端口转发隧道，通过 QUIC 加密通道访问远程主机的内部服务（如数据库、Redis、内部 API）。在本地指定地址和端口开启 TCP 监听，将连接转发到远程目标。如果主机配置了 `allowed_tunnel_targets` 白名单，只有匹配的目标才允许创建隧道（不配置则全部允许）。

| 参数 | 类型 | 必填 | 默认值 | 说明 |
|------|------|:---:|--------|------|
| `host` | string | ✅ | — | 远程主机名 |
| `local_port` | integer | ✅ | — | 本地监听端口 |
| `remote_host` | string | ✅ | — | 远程目标主机（可以是内网地址） |
| `remote_port` | integer | ✅ | — | 远程目标端口 |
| `local_addr` | string | | `127.0.0.1` | 本地监听地址 |

**返回**

```json
{
  "ok": true,
  "tunnel_id": "t_abc123",
  "local_addr": "127.0.0.1:15432",
  "remote": "db-server:5432"
}
```

**使用示例**

```
// 访问远程数据库
tunnel_create host="tf01" local_port=15432 remote_host="db-server" remote_port=5432
// 然后通过 localhost:15432 连接 PostgreSQL

// 访问远程内网 API
tunnel_create host="tf01" local_port=8080 remote_host="api.internal" remote_port=8080
// 然后通过 localhost:8080 访问 API
```

### `tunnel_list`

列出所有活跃的端口转发隧道。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| 无 | | |

**返回**

```json
{
  "ok": true,
  "tunnels": [
    {
      "tunnel_id": "t_abc123",
      "local_addr": "127.0.0.1:15432",
      "local_port": 15432,
      "remote_host": "db-server",
      "remote_port": 5432,
      "created_at": "2026-07-03T12:34:56Z",
      "active_connections": 3
    }
  ],
  "count": 1
}
```

### `tunnel_close`

关闭指定的端口转发隧道。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `tunnel_id` | string | ✅ | 隧道 ID（由 tunnel_create 返回） |

**返回**

```json
{
  "ok": true,
  "closed": "t_abc123"
}
```

---

## 部署升级

### `deploy_bridge`

将编译好的 rmux-bridge 二进制部署到远程主机并重启服务。仅支持升级场景（目标主机必须已有 bridge 运行）。首次部署请通过 SSH 执行 `deploy/install-bridge.sh`。

内部流程：查询 systemd ExecStart → 校验路径 → 上传二进制 → 设权限 → 替换 → nohup 后台重启。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:---:|------|
| `hosts` | string[] | ✅ | 目标主机名 |
| `binary_path` | string | ✅ | 本地编译好的 bridge 二进制路径 |
| `remote_path` | string | | 远程目标路径（不传则从 systemd 自动获取） |
| `concurrency` | integer | | 最大并发数，默认 3 |

**返回**

```json
{
  "ok": true,
  "binary": "target/x86_64-unknown-linux-musl/release/rmux-bridge",
  "binary_size": 11052600,
  "total": 2,
  "success": 2,
  "failed": 0,
  "total_duration_ms": 3899,
  "results": {
    "tf001": { "ok": true, "status": "restarted", "output": "deployed", "exit_code": 0 }
  }
}
```

| status | 含义 |
|--------|------|
| `restarted` | ✅ 部署成功，服务已重启 |
| `first_time_deploy` | systemd unit 不存在，走 SSH 首次部署 |
| `host_not_found` | 主机不在 registry 中 |
| `bridge_unreachable` | 无法连接目标主机 |
| `session_failed` | 创建 session 失败 |
| `path_mismatch` | 指定的路径与 systemd ExecStart 不一致 |
| `upload_failed` | 文件传输失败 |
| `replace_failed` | 替换二进制文件失败 |
| `reconnect_failed` | 重启后无法重连 bridge |
| `verify_failed` | 重启后验证 service 状态失败 |

---

## 审计与录制

### `audit_query`

查询 Server 侧集中审计日志。记录所有 MCP 工具调用（谁、什么时间、哪台机器、什么操作、是否成功）。Central Server 模式下查询操作历史的首选工具。

| 参数 | 类型 | 必填 | 说明 |
|------|------|:----:|------|
| `host` | string | | 按主机名过滤 |
| `action` | string | | 按操作类型过滤（如 Exec, SessionCreate） |
| `agent` | string | | 按 agent 名过滤 |
| `since` | string | | 起始时间（RFC3339） |
| `until` | string | | 截止时间（RFC3339） |
| `success` | boolean | | 按成功/失败过滤 |
| `limit` | integer | | 返回条数上限（省略时返回全部匹配事件） |

### `query_bridge_audit`

查询目标主机 bridge 侧的连接事件日志（认证失败、CLI 会话 attach/detach/exit、录制清理等）。

**参数**

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `host` | string | ✅ | 目标主机名 |
| `event_type` | string | | 事件类型过滤：`auth_failure`, `attach`, `detach`, `exit`, `recording_cleanup` |
| `session_name` | string | | 会话名过滤 |
| `since` | string | | 起始时间（RFC3339） |
| `until` | string | | 截止时间（RFC3339） |
| `limit` | integer | | 返回条数上限，默认 50 |

**返回**

```json
{
  "events": [
    {
      "event_id": "019f8a6f-810c-70d3-b302-52e61503e5d2",
      "timestamp": "2026-07-22T15:26:31+00:00",
      "client_addr": "10.230.21.231:52743",
      "event_type": "attach",
      "session_name": "yunying",
      "pane_id": "%0",
      "detail": null,
      "duration_secs": null,
      "exit_code": null
    }
  ]
}
```

---

### `list_recordings`

列出已同步到本地的 PTY 会话录制文件（asciinema v2 格式）。

**参数**

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `host` | string | | 按主机名过滤 |
| `date` | string | | 按日期过滤（YYYY-MM-DD） |
| `session` | string | | 按会话名前缀过滤 |

**返回**

```json
{
  "recordings": [
    {
      "host": "tf001",
      "date": "2026-07-22",
      "file": "yunying__0_1784733267_4899.cast",
      "size_bytes": 199466,
      "path": "/Users/xxx/.yunying/recordings/tf001/2026-07-22/yunying__0_1784733267_4899.cast"
    }
  ],
  "count": 1
}
```

---

### `get_recording`

获取指定录制文件的内容（asciinema v2 JSONL 格式）。访问本身会被审计记录。

**参数**

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `path` | string | ✅ | 录制文件路径（从 `list_recordings` 获取） |

**返回**

```json
{
  "path": "/Users/xxx/.yunying/recordings/tf001/2026-07-22/yunying__0_1784733267_4899.cast",
  "content": "{\"version\":2,...}\n[0.011, \"o\", \"...\"]\n..."
}
```

**安全限制**：路径必须在 recordings 目录内，路径穿越会被拒绝。

**回放方式**：使用 CLI `yunying-cli replay <file.cast> [--speed 2.0] [--idle 1.0]` 或第三方工具 `asciinema play`。

---

## 系统

### `yunying_usage_rules`

⚠️ 占位工具，**不要调用**。使用规则已内置于 MCP server instructions，无需通过工具获取。

| 参数 | 类型 | 必填 |
|------|------|:---:|
| 无 | | |

**返回** `{}` — 空对象。
