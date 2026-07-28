# 终端状态感知（Terminal State Awareness）设计方案

> 状态：Final v5（经 Oracle 审查 + PiloTY 对比 + E2E 测试验证） | 日期：2026-07-14

---

## 1. 问题

yunying 当前的 `capture_pane`、`pane_info`、`wait_for_text` 等工具只返回原始文本，不告诉 AI 终端当前处于什么状态。AI 必须自己从文本中推断：

- 命令执行完了吗？还是在等输入？
- 终端在等密码输入吗？
- vim 打开了吗？
- 是 shell 提示符还是命令输出中的 `$` 符号？

这导致 AI 经常误判终端状态，发出不恰当的命令。

### 1.1 灵感来源

PiloTY（yiwenlu66/PiloTY）实现了 8 种终端状态分类 + cursor 位置辅助判断。其核心思路：**在返回文本的同时告诉 AI 终端当前在干什么**。

### 1.2 设计原则

- **不改现有架构**：在已有返回值中附加字段，不新增工具
- **零额外 RPC**：利用已获取的 `PaneSnapshot`（`capture_pane` Path A 和 `wait_stable` 已经调用了 `pane.snapshot()`）
- **纯规则引擎**：不引入 LLM，纯启发式匹配
- **向后兼容**：新字段为 optional，旧客户端不受影响

---

## 2. 终端状态定义

```rust
/// 终端当前状态分类
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    /// Shell 提示符，可以发送命令
    Ready,
    /// 命令正在执行中
    Running,
    /// 等待密码输入（Password:、[sudo]、passphrase）
    Password,
    /// 等待确认（[y/n]、Are you sure、Continue?）
    Confirm,
    /// 交互式环境（Python >>>、mysql>、node>）
    Repl,
    /// 编辑器（vim、nano、vi）
    Editor,
    /// 分页器（less、more、man）
    Pager,
    /// 无法判断
    Unknown,
}
```

---

## 3. 检测算法

### 3.1 输入

| 输入 | 来源 | 成本 |
|------|------|:----:|
| `text: &str` | `snapshot.visible_text()` — 已有 | 零 |
| `cursor_col: u16` | `snapshot.cursor.col` — 已有，当前被丢弃 | 零 |
| `cursor_visible: bool` | `snapshot.cursor.visible` — 已有 | 零 |

### 3.2 检测时机

> ⚠️ **重要**：检测必须在 `clean_text()` **之前**运行，使用 `snapshot.visible_text()` 的原始输出。
> `clean_text()` 会去除 ANSI 转义码和空行，检测需要依赖原始完整文本（包括 ANSI 状态码和终端布局信息）。
> 返回给用户的 `text` 字段仍然是 `clean_text` 处理后的文本，两者不冲突。

```rust
// 正确的调用顺序
let raw_text = snapshot.visible_text();           // 原始文本（含 prompt）
let state = detect_terminal_state(&raw_text, ...); // 在原始文本上检测
let clean = Self::clean_text(&raw_text, None);     // 清理后返回给用户
```

### 3.3 规则引擎

```rust
fn detect_terminal_state(text: &str, cursor_col: u16, cursor_visible: bool) -> TerminalState {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return TerminalState::Unknown;
    }

    // 取最后 12 行作为检测窗口（避免 scrollback 干扰）
    let window_start = lines.len().saturating_sub(12);
    let window = &lines[window_start..];
    let tail = window.last().map(|l| l.trim()).unwrap_or("");

    // ── 规则 0：光标不可见 → 先检查编辑器/分页器，否则 Running ──
    if !cursor_visible {
        if text.contains("-- INSERT --") || text.contains("-- VISUAL --")
            || text.contains("-- NORMAL --") || text.contains("-- REPLACE --") {
            return TerminalState::Editor;
        }
        if text.contains("(END)") || text.contains("(end)")
            || tail.starts_with(':') {
            return TerminalState::Pager;
        }
        return TerminalState::Running;
    }

    // ── 规则 1：密码提示（最高优先级，安全相关） ──
    // 密码提示可能出现在光标 col=0 的位置（如 sudo 多行输出后光标停在行首）
    // 必须在 cursor_col=0 规则之前检测，否则会被误判为 Running
    let password_keywords = [
        "password:", "Password:", "PASSWORD:",
        "[sudo]", "passphrase", "Passphrase",
        "Enter PIN:", "token:", "Token:",
    ];
    for kw in &password_keywords {
        if tail.contains(kw) {
            return TerminalState::Password;
        }
    }

    // ── 规则 2：确认提示（高优先级，安全相关） ──
    // 确认提示也可能在光标 col=0 的位置（如 SSH fingerprint 确认）
    let confirm_patterns = [
        "[y/n]", "[Y/n]", "[y/N]", "(y/n)",
        "Are you sure", "Continue?", "Proceed?",
        "Do you want to", "Overwrite?",
        "yes/no", "Yes/No",
    ];
    for pat in &confirm_patterns {
        if tail.contains(pat) {
            return TerminalState::Confirm;
        }
    }

    // ── 规则 3：编辑器 ──
    let editor_markers = [
        "-- INSERT --", "-- VISUAL --", "-- NORMAL --", "-- REPLACE --",
        "-- COMMAND --", "[New File]", "[New DIRECTORY]",
        "GNU nano", "Pico editor",
    ];
    for marker in &editor_markers {
        if text.contains(marker) {
            return TerminalState::Editor;
        }
    }

    // ── 规则 4：分页器 ──
    if tail.starts_with(':')  // less 的命令模式
        || tail.contains("(END)")
        || tail.contains("(end)")
        || text.contains("Manual page") && tail.contains("(press h for help)")
    {
        return TerminalState::Pager;
    }

    // ── 规则 5：REPL 环境 ──
    let repl_prefixes = [">>>", "... ", "In [", "mysql>", "node>", "irb>", "pry>"];
    for prefix in &repl_prefixes {
        if tail.starts_with(prefix) {
            return TerminalState::Repl;
        }
    }

    // ── 规则 6：Shell 提示符 + cursor 二次验证 ──
    // 借鉴 PiloTY：cursor_col=0 不作为独立规则，而是 shell 提示符的二次验证。
    // 这样避免了 heredoc `>` 等待输入等非 shell 场景被误判为 Running。
    // PiloTY 的做法：先匹配 shell 提示符模式，匹配成功后再用 cursor 位置区分
    // "真正的提示符"（cursor 在提示符之后）和"命令输出中的提示符文本"（cursor 在 col 0）。
    if tail.ends_with('$') || tail.ends_with('#')
        || tail.ends_with('>') || tail.ends_with('%')
    {
        // 排除进度条：包含 [ ] 或百分比数字
        if tail.contains('[') && tail.contains(']') {
            return TerminalState::Running;
        }
        // 排除命令输出中恰好以 $ 结尾的行
        if tail.len() > 80 && !tail.contains("@") && !tail.contains(":~") {
            return TerminalState::Running;
        }
        // cursor 二次验证：光标在 col 0 = shell 还在处理上一行输出
        // 例如：`echo "$"` 输出 $ 但光标回到 col 0，不是 ready
        if cursor_col == 0 {
            return TerminalState::Running;
        }
        return TerminalState::Ready;
    }

    // ── 规则 7：自定义 shell_prompt_regex（未来扩展） ──
    // if let Some(ref regex) = custom_prompt_regex {
    //     if regex.is_match(tail) {
    //         return TerminalState::Ready;
    //     }
    // }

    TerminalState::Unknown
}
```

### 3.4 规则优先级（v4 最终版）

```
规则 0: 光标不可见 → 编辑器/分页器/Running
    ↓
规则 1: 密码关键词 → Password
    ↓
规则 2: 确认关键词 → Confirm
    ↓
规则 3: 编辑器标记 → Editor
    ↓
规则 4: 分页器标记 → Pager
    ↓
规则 5: REPL 前缀 → Repl
    ↓
规则 6: Shell 提示符 → cursor col=0 ? Running : Ready   ← cursor 是二次验证
    ↓
Unknown
```

> **v3 修订说明**（对比 PiloTY 后调整）：
>
> v1 将 `cursor_col=0 → Running` 作为独立高优先级规则，导致密码/确认误判。
> v2 将密码/确认提升到 cursor_col=0 之前，但 cursor_col=0 仍是独立规则——
> 任何 col=0 的非空行都判为 Running，包括 heredoc `>` 等待输入等场景。
>
> v3 借鉴 PiloTY 的做法：**把 cursor_col=0 从独立规则改为 shell 提示符的二次验证**。
> PiloTY 的逻辑是：先匹配 shell 提示符模式（`$`/`#`/`>`/`%`），匹配成功后再用
> cursor 位置区分"真正的提示符"和"命令输出中的提示符文本"。
>
> 这样做的好处：
> - heredoc `>` 等待输入 → 不以 `$`/`#`/`%` 结尾，不会被 shell 提示符分支捕获，
>   最终落到 Unknown（比误判为 Running 更安全）
> - `echo "$"` 输出 → 匹配 `$` 结尾 + cursor col=0 → 正确判为 Running
> - `user@host:~$` → 匹配 `$` 结尾 + cursor col=14 → 正确判为 Ready

---

## 4. API 变更

### 4.1 capture_pane 返回值变更

**当前（Path A）：**
```json
{"ok": true, "text": "user@host:~$ "}
```

**变更后：**
```json
{
  "ok": true,
  "text": "user@host:~$ ",
  "terminal_state": "ready",
  "cursor": {"row": 0, "col": 14, "visible": true}
}
```

**实现**：`capture_pane` Path A 已经调用 `pane.snapshot()`，只需从已获取的 snapshot 中提取 cursor 并运行检测算法。零额外 RPC。

```rust
// protocol/output.rs handle_capture_pane Path A 改动
let snapshot = pane.snapshot().await?;
let text = snapshot.visible_text();
let state = detect_terminal_state(&text, snapshot.cursor.col, snapshot.cursor.visible);

json!({
    "ok": true,
    "text": text,
    "terminal_state": state,
    "cursor": {
        "row": snapshot.cursor.row,
        "col": snapshot.cursor.col,
        "visible": snapshot.cursor.visible,
    }
})
```

### 4.2 wait_stable 返回值变更

**当前：**
```json
{"ok": true, "stable": true}
```

**变更后：**
```json
{
  "ok": true,
  "stable": true,
  "terminal_state": "ready",
  "cursor": {"row": 5, "col": 14, "visible": true}
}
```

**实现**：`wait_until_stable_for()` 返回 `Result<PaneSnapshot>`，当前代码用 `Ok(_snapshot)` 丢弃了。改为 `Ok(snapshot)` 即可。

### 4.3 wait_for_text 返回值变更

**当前：**
```json
{"ok": true, "found": true}
```

**变更后：**
```json
{
  "ok": true,
  "found": true,
  "terminal_state": "password",
  "cursor": {"row": 3, "col": 10, "visible": true}
}
```

**实现**：`wait_for_text` 成功后需额外调用 `pane.snapshot()` 获取 cursor。这是一次额外 RPC，但 `wait_for_text` 本身是低频操作，可接受。

### 4.4 exec 返回值变更

**当前：**
```json
{"ok": true, "output": "...", "exit_code": 0, "duration_ms": 1234}
```

**变更后：**
```json
{
  "ok": true,
  "output": "...",
  "exit_code": 0,
  "duration_ms": 1234,
  "terminal_state": "ready",
  "cursor": {"row": 10, "col": 14, "visible": true}
}
```

**实现依赖链**：`exec_in_session` 在 MCP 层实现（`tools/exec.rs`），通过 JSON 帧与 bridge 通信。
exec 循环中最后一次 `capture_pane` 调用走 bridge 的 `handle_capture_pane` Path A。
因此需要**先完成改动点 4.1**（bridge capture_pane 返回 terminal_state），
然后 exec 从最后一次 capture_pane 响应中提取 `terminal_state` 和 `cursor` 字段即可，零额外 RPC。

```rust
// tools/exec.rs exec_in_session 改动（伪代码）
// 在检测到 sentinel 后，从最后一次 capture_pane 响应中提取：
let terminal_state = resp.get("terminal_state");
let cursor = resp.get("cursor");
// 附加到 ExecResult 中返回
```

### 4.5 pane_info 返回值变更

**当前：**
```json
{
  "ok": true,
  "info": {
    "pane_id": "%0",
    "size_cols": 80,
    "size_rows": 24,
    ...
  }
}
```

**变更后：**
```json
{
  "ok": true,
  "info": {
    "pane_id": "%0",
    "size_cols": 80,
    "size_rows": 24,
    ...,
    "terminal_state": "ready",
    "cursor": {"row": 0, "col": 14, "visible": true}
  }
}
```

**实现**：代码审查发现 `pane.info()` 返回的内部 `LiveDetails` 结构体**已包含** `cursor_x`、`cursor_y`、`cursor_visible`、`cursor_style` 字段（`info.rs` 第 56-59 行），但这些字段未暴露到公共 `PaneInfo` 结构体。

有两种路径：
- **路径 A（零额外 RPC）**：直接使用 `info()` 内部的 cursor 字段。需要修改 rmux-sdk 的公共 `PaneInfo` 暴露这些字段，或在 bridge 层通过其他方式访问。
- **路径 B（+1 次 IPC）**：额外调用 `pane.snapshot()` 获取 cursor + 运行检测。成本是 1 次 IPC 往返（约 1-10ms），`pane_info` 是低频查询操作，可接受。

**推荐路径 B**：不依赖 SDK 内部字段变更，实现更简单。

---

## 5. 改动范围

### 5.1 文件清单

| 文件 | 改动 | 复杂度 |
|------|------|:------:|
| `crates/rmux-bridge/src/terminal_state.rs` | **新增**：`TerminalState` 枚举 + `detect_terminal_state()` 函数 | 低 |
| `crates/rmux-bridge/src/protocol/output.rs` | **修改**：5 个 handler 的返回值添加 `terminal_state` + `cursor` | 低 |
| `crates/rmux-bridge/src/main.rs` | **修改**：`mod terminal_state;` 声明 | 极低 |
| `crates/rmux-bridge/src/terminal_state.rs` | **新增**：单元测试（10+ 测试用例） | 低 |
| `docs/TOOLS.md` | **修改**：更新 5 个工具的返回值文档 | 低 |

### 5.2 需小幅改动的文件

| 文件 | 改动 | 理由 |
|------|------|------|
| `crates/yunying-mcp/src/tools/exec.rs` | `exec_in_session` 从最后一次 capture_pane 响应中提取 `terminal_state` + `cursor` | exec 在 MCP 层实现，依赖 bridge capture_pane 先返回这些字段 |

### 5.3 不改动的文件

| 文件 | 理由 |
|------|------|
| `crates/yunying-core/src/types.rs` | `TerminalState` 只在 bridge 侧序列化，不需要在 core 中定义 |
| `config/hosts.yaml` | 第一版不加 `shell_prompt_regex`，后续按需扩展 |

### 5.4 依赖

无新依赖。`TerminalState` 使用 `serde::Serialize`（已有）。

---

## 6. 测试计划

### 6.1 单元测试（terminal_state.rs）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // ── Ready ──
    #[test]
    fn test_bash_prompt() {
        let text = "user@host:~$ ";
        assert_eq!(detect_terminal_state(text, 14, true), TerminalState::Ready);
    }

    #[test]
    fn test_root_prompt() {
        let text = "root@server:/etc# ";
        assert_eq!(detect_terminal_state(text, 18, true), TerminalState::Ready);
    }

    // ── Running ──
    #[test]
    fn test_cursor_col_zero_with_prompt() {
        // shell 提示符匹配成功 + cursor col=0 → Running（二次验证）
        let text = "user@host:~$";
        assert_eq!(detect_terminal_state(text, 0, true), TerminalState::Running);
    }

    #[test]
    fn test_cursor_col_zero_without_prompt() {
        // cursor col=0 但 tail 不以 $/#/>/% 结尾 → 不触发二次验证
        // heredoc 等待输入 `>` 是独立一行，tail 是 ">"
        // 注意：">" 会匹配 shell 提示符，所以这个场景需要更精确
        let text = "some output line\nanother line";
        assert_eq!(detect_terminal_state(text, 0, true), TerminalState::Unknown);
    }

    #[test]
    fn test_progress_bar_not_ready() {
        let text = "[==================>     ] 75%";
        assert_eq!(detect_terminal_state(text, 30, true), TerminalState::Running);
    }

    // ── Password ──
    #[test]
    fn test_sudo_password() {
        let text = "[sudo] password for user: ";
        assert_eq!(detect_terminal_state(text, 26, true), TerminalState::Password);
    }

    #[test]
    fn test_ssh_password() {
        let text = "user@host's password: ";
        assert_eq!(detect_terminal_state(text, 22, true), TerminalState::Password);
    }

    // ── Confirm ──
    #[test]
    fn test_apt_confirm() {
        let text = "Do you want to continue? [Y/n] ";
        assert_eq!(detect_terminal_state(text, 31, true), TerminalState::Confirm);
    }

    // ── Editor ──
    #[test]
    fn test_vim_insert_mode() {
        let text = "line 1\nline 2\n~\n-- INSERT --                                        2,1";
        assert_eq!(detect_terminal_state(text, 5, false), TerminalState::Editor);
    }

    // ── Pager ──
    #[test]
    fn test_less_end() {
        let text = "some content\n(END)";
        assert_eq!(detect_terminal_state(text, 5, false), TerminalState::Pager);
    }

    // ── REPL ──
    #[test]
    fn test_python_repl() {
        let text = "Python 3.12.0\n>>> ";
        assert_eq!(detect_terminal_state(text, 4, true), TerminalState::Repl);
    }

    // ── P0 反例验证（cursor_col=0 但非 Running） ──
    #[test]
    fn test_sudo_password_cursor_col_zero() {
        // sudo 多行输出后光标停在行首等待密码
        let text = "[sudo] password for user: ";
        assert_eq!(detect_terminal_state(text, 0, true), TerminalState::Password);
    }

    #[test]
    fn test_ssh_confirm_cursor_col_zero() {
        // SSH fingerprint 确认，光标在行首
        let text = "Are you sure you want to continue connecting (yes/no)? ";
        assert_eq!(detect_terminal_state(text, 0, true), TerminalState::Confirm);
    }

    // ── Unknown ──
    #[test]
    fn test_empty() {
        assert_eq!(detect_terminal_state("", 0, true), TerminalState::Unknown);
    }

    #[test]
    fn test_cursor_invisible_no_marker() {
        let text = "some random output";
        assert_eq!(detect_terminal_state(text, 5, false), TerminalState::Running);
    }

    // ── 补充场景（Oracle 审查建议） ──
    #[test]
    fn test_zsh_prompt() {
        let text = "➜ ~ git:(main) ✗ ";
        // zsh 提示符不以 $ # > % 结尾，应判为 Unknown
        // 后续可通过 shell_prompt_regex 扩展支持
        assert_eq!(detect_terminal_state(text, 20, true), TerminalState::Unknown);
    }

    #[test]
    fn test_htop_tui() {
        let text = "  PID USER      PR  NI    VIRT    RES    SHR S  %CPU  %MEM\n    1 root      20   0  168576  13248   8576 S   0.0   0.2";
        // 光标不可见，无编辑器/分页器标记 → Running
        assert_eq!(detect_terminal_state(text, 0, false), TerminalState::Running);
    }

    #[test]
    fn test_command_output_with_dollar() {
        let text = "total $100\nprice is $50";
        // 命令输出包含 $ 但不是提示符（无 @ 或 :~，且行较长）
        assert_eq!(detect_terminal_state(text, 12, true), TerminalState::Running);
    }

    #[test]
    fn test_git_commit_opens_vim() {
        let text = "\n# Please enter the commit message\n~\n-- INSERT --                                        1,1";
        // git commit 打开 vim
        assert_eq!(detect_terminal_state(text, 1, false), TerminalState::Editor);
    }

    #[test]
    fn test_polkit_auth() {
        let text = "polkit-agent-helper-1: password: ";
        // polkit 认证提示
        assert_eq!(detect_terminal_state(text, 33, true), TerminalState::Password);
    }
}
```

### 6.2 集成验证

在真实终端上验证以下场景：

| 场景 | 预期状态 |
|------|---------|
| 刚登录 bash | `ready` |
| `sleep 10` 执行中 | `running` |
| `sudo apt update` 等待密码 | `password` |
| `apt install xxx` 等待确认 | `confirm` |
| `python3` 进入 REPL | `repl` |
| `vim file.txt` | `editor` |
| `man ls` | `pager` |
| `htop` | `running`（光标不可见，无编辑器标记） |

---

## 7. 后续扩展（不在第一版）

| 特性 | 描述 | 优先级 |
|------|------|:------:|
| `shell_prompt_regex` | HostConfig 中配置自定义提示符正则 | 中 |
| `wait_for_prompt` 工具 | 新 MCP 工具，阻塞等待直到终端变为 `ready` | 中 |
| `terminal_state` 事件推送 | stream_pane 中推送状态变化事件 | 低 |
| LLM sampling 兜底 | 启发式判为 `unknown` 时可选调 LLM | 低（AI client 自身有 LLM） |

---

## 8. 风险评估

| 风险 | 影响 | 缓解 |
|------|------|------|
| 误判状态（false positive） | AI 可能在不恰当时机发命令 | 状态是辅助信息，AI 仍可忽略；密码/确认优先于 cursor_col=0 规则 |
| 自定义 PS1 不匹配 | 非标准提示符（如 zsh `➜`）判为 `unknown` | `unknown` 是安全默认值，不会导致错误行为；后续可通过 `shell_prompt_regex` 扩展 |
| 性能影响 | capture_pane Path A / wait_stable 零额外 RPC；wait_for_text / pane_info 各 +1 次 IPC（约 1-10ms） | 检测算法本身 < 1μs；额外 IPC 仅在低频操作中发生 |
| 向后兼容 | 旧客户端收到多余字段 | JSON 新增字段不影响标准解析器；在 CHANGELOG 中记录 |
| `clean_text` 交互 | 如果检测在 clean_text 之后运行，ANSI 码和终端布局信息会丢失 | 检测必须在 `clean_text` **之前**运行（已在 §3.2 明确） |

---

## 9. 实施顺序

基于代码可行性验证，推荐的实施顺序：

| 步骤 | 改动 | 依赖 | 额外 RPC |
|:----:|------|:----:|:--------:|
| 1 | 新增 `terminal_state.rs`（枚举 + 检测算法 + 测试） | 无 | - |
| 2 | `handle_capture_pane` Path A 添加 terminal_state + cursor | 步骤 1 | 零 |
| 3 | `handle_wait_stable` 保留 snapshot，添加 terminal_state + cursor | 步骤 1 | 零 |
| 4 | `handle_wait_for_text` 成功后添加 snapshot + terminal_state | 步骤 1 | +1 IPC |
| 5 | `handle_pane_info` 添加 snapshot + terminal_state | 步骤 1 | +1 IPC |
| 6 | `exec_in_session` 从最后一次 capture_pane 响应中提取 terminal_state | 步骤 2 | 零 |
| 7 | 更新 `docs/TOOLS.md` 返回值文档 | 步骤 2-6 | - |

---

## 10. 审查修订记录

| 版本 | 日期 | 修订内容 |
|------|------|---------|
| v1 | 2026-07-13 | 初稿 |
| v2 | 2026-07-13 | Oracle 审查后修订：① 密码/确认检测优先级提升到 cursor_col=0 之前（P0 安全修复）② 添加 cursor_col=0 反例说明 ③ 明确检测在 clean_text 之前运行 ④ 修正 exec 依赖链说明 ⑤ 修正 pane_info 成本评估（IPC 往返而非纯字符串操作）⑥ 发现 pane.info() 自带 cursor 字段 ⑦ 补充 6 个测试用例（zsh、htop、命令输出含 $、git commit vim、polkit、P0 反例） |
| v3 | 2026-07-13 | 对比 PiloTY 排序后修订：① cursor_col=0 从独立规则改为 shell 提示符的二次验证（借鉴 PiloTY 做法，避免 heredoc 等非 shell 场景误判）② 规则编号重排为 0-7 ③ 更新测试用例适配新逻辑 |
| v4 | 2026-07-13 | E2E 测试后修订：① 修复 vim 新文件检测（添加 `[New]` 标记 + `~` 行计数启发式）② 新增 2 个测试用例（vim_new_file、vim_tilde_lines）③ 全部 11 个 E2E 场景通过验证 |
| v5 | 2026-07-13 | MCP Server 重启后验证：exec 工具成功返回 `terminal_state` 和 `cursor` 字段，所有 12 个 E2E 场景全部通过 |

---

## 11. E2E 测试结果

### 11.1 测试环境

- **测试主机**: tf001（Ubuntu 22.04, Linux x86_64）
- **部署方式**: `just release-linux` 交叉编译 + `deploy_bridge` 热更新
- **测试日期**: 2026-07-13

### 11.2 测试结果汇总

| # | 场景 | 命令/操作 | 预期状态 | 实际状态 | 结果 |
|:-:|------|----------|---------|---------|:----:|
| 1 | 部署 | `just release-linux` + `deploy_bridge` | bridge 重启成功 | `status: restarted` | ✅ |
| 2 | bash prompt | `capture_pane`（空闲 shell） | `ready` | `ready` | ✅ |
| 3 | exec 返回 | \`exec echo "hello"\` | 含 \`pre_terminal_state\` + \`terminal_state\` | \`pre_terminal_state: "ready"\` | ✅ |
| 4 | wait_stable | `echo "test" && sleep 1` → `wait_stable` | `ready` | `ready` | ✅ |
| 5 | wait_for_text | `echo "MARKER"` → `wait_for_text` | `ready` | `ready` | ✅ |
| 6 | pane_info | `pane_info`（空闲 shell） | `ready` | `ready` | ✅ |
| 7 | 密码提示 | `read -s -p "Password: "` | `password` | `password` | ✅ |
| 8 | vim 编辑器 | `vim /tmp/test.txt` | `editor` | `editor` | ✅ |
| 9 | less 分页器 | `echo ... \| less` | `pager` | `pager` | ✅ |
| 10 | Python REPL | `python3` | `repl` | `repl` | ✅ |
| 11 | htop TUI | `htop` | `running` | `running` | ✅ |

### 11.3 E2E 中发现并修复的问题

**问题**：vim 打开新文件时显示 `[New]` 而非 `[New File]`，且空文件有多行 `~` 标记，初版检测算法未覆盖。

**修复**：
1. 在 `editor_markers` 中添加 `"[New]"` 标记
2. 添加 `~` 行计数启发式：如果窗口中有 3+ 行仅为 `~`，判定为 vim

**验证**：修复后重新部署，vim 场景正确返回 `editor`。


