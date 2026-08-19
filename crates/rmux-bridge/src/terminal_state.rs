use serde::Serialize;

/// 终端当前状态分类
#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// 检测终端当前状态
///
/// 基于终端可见文本和光标位置，使用启发式规则判断终端状态。
/// 检测基于原始 `visible_text()` 输出，在 `clean_text()` 去除 ANSI 码和空行之前运行。
pub fn detect_terminal_state(text: &str, cursor_col: u16, cursor_visible: bool) -> TerminalState {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return TerminalState::Unknown;
    }

    // 取最后 12 行作为检测窗口（避免 scrollback 干扰）
    // 但跳过尾部空行，避免 prompt 在空行之前被漏掉
    let mut effective_end = lines.len();
    while effective_end > 0 && lines[effective_end - 1].trim().is_empty() {
        effective_end -= 1;
    }
    if effective_end == 0 {
        return TerminalState::Unknown;
    }
    let window_start = effective_end.saturating_sub(12);
    let window = &lines[window_start..effective_end];
    let tail = window.last().map(|l| l.trim()).unwrap_or("");

    // ── 规则 0：光标不可见 → 先检查编辑器/分页器，否则 Running ──
    if !cursor_visible {
        if text.contains("-- INSERT --")
            || text.contains("-- VISUAL --")
            || text.contains("-- NORMAL --")
            || text.contains("-- REPLACE --")
        {
            return TerminalState::Editor;
        }
        if text.contains("(END)") || text.contains("(end)") || tail.starts_with(':') {
            return TerminalState::Pager;
        }
        return TerminalState::Running;
    }

    // ── 规则 1：密码提示（最高优先级，安全相关） ──
    let password_keywords = [
        "password:",
        "Password:",
        "PASSWORD:",
        "[sudo]",
        "passphrase",
        "Passphrase",
        "Enter PIN:",
        "token:",
        "Token:",
    ];
    for kw in &password_keywords {
        if tail.contains(kw) {
            return TerminalState::Password;
        }
    }

    // ── 规则 2：确认提示（高优先级，安全相关） ──
    let confirm_patterns = [
        "[y/n]",
        "[Y/n]",
        "[y/N]",
        "(y/n)",
        "Are you sure",
        "Continue?",
        "Proceed?",
        "Do you want to",
        "Overwrite?",
        "yes/no",
        "Yes/No",
    ];
    for pat in &confirm_patterns {
        if tail.contains(pat) {
            return TerminalState::Confirm;
        }
    }

    // ── 规则 3：编辑器 ──
    let editor_markers = [
        "-- INSERT --",
        "-- VISUAL --",
        "-- NORMAL --",
        "-- REPLACE --",
        "-- COMMAND --",
        "[New File]",
        "[New DIRECTORY]",
        "[New]", // vim 新文件指示器
        "GNU nano",
        "Pico editor",
    ];
    for marker in &editor_markers {
        if text.contains(marker) {
            return TerminalState::Editor;
        }
    }
    // vim 空行标记：如果窗口中有 3+ 行仅为 "~"，大概率是 vim
    let tilde_lines = window.iter().filter(|l| l.trim() == "~").count();
    if tilde_lines >= 3 {
        return TerminalState::Editor;
    }

    // ── 规则 4：分页器 ──
    if tail.starts_with(':')
        || tail.contains("(END)")
        || tail.contains("(end)")
        || (text.contains("Manual page") && tail.contains("(press h for help)"))
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
    // 传统 bash/sh/zsh 提示符以 $ # > % 结尾；主题化 zsh（agnoster/p10k）以 ➜ ❯ 开头
    let shell_prompt = tail.ends_with('$')
        || tail.ends_with('#')
        || tail.ends_with('>')
        || tail.ends_with('%')
        || tail.starts_with('➜')
        || tail.starts_with('❯');
    if shell_prompt {
        // 排除进度条
        if tail.contains('[') && tail.contains(']') {
            return TerminalState::Running;
        }
        // 排除长行命令输出
        if tail.len() > 80 && !tail.contains('@') && !tail.contains(":~") {
            return TerminalState::Running;
        }
        // cursor 二次验证：光标在 col 0 = shell 还在处理
        if cursor_col == 0 {
            return TerminalState::Running;
        }
        return TerminalState::Ready;
    }

    TerminalState::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Ready ──
    #[test]
    fn test_bash_prompt() {
        assert_eq!(
            detect_terminal_state("user@host:~$ ", 14, true),
            TerminalState::Ready
        );
    }

    #[test]
    fn test_root_prompt() {
        assert_eq!(
            detect_terminal_state("root@server:/etc# ", 18, true),
            TerminalState::Ready
        );
    }

    // ── Running ──
    #[test]
    fn test_cursor_col_zero_with_prompt() {
        // shell 提示符匹配成功 + cursor col=0 → Running（二次验证）
        assert_eq!(
            detect_terminal_state("user@host:~$", 0, true),
            TerminalState::Running
        );
    }

    #[test]
    fn test_cursor_col_zero_without_prompt() {
        // cursor col=0 但 tail 不以 $/#/>/% 结尾 → Unknown
        assert_eq!(
            detect_terminal_state("some output line\nanother line", 0, true),
            TerminalState::Unknown
        );
    }

    #[test]
    fn test_progress_bar_not_ready() {
        assert_eq!(
            detect_terminal_state("[==================>     ] 75%", 30, true),
            TerminalState::Running
        );
    }

    // ── Password ──
    #[test]
    fn test_sudo_password() {
        assert_eq!(
            detect_terminal_state("[sudo] password for user: ", 26, true),
            TerminalState::Password
        );
    }

    #[test]
    fn test_ssh_password() {
        assert_eq!(
            detect_terminal_state("user@host's password: ", 22, true),
            TerminalState::Password
        );
    }

    #[test]
    fn test_sudo_password_cursor_col_zero() {
        // P0 反例：sudo 密码提示光标在行首，必须判为 Password 而非 Running
        assert_eq!(
            detect_terminal_state("[sudo] password for user: ", 0, true),
            TerminalState::Password
        );
    }

    #[test]
    fn test_polkit_auth() {
        assert_eq!(
            detect_terminal_state("polkit-agent-helper-1: password: ", 33, true),
            TerminalState::Password
        );
    }

    // ── Confirm ──
    #[test]
    fn test_apt_confirm() {
        assert_eq!(
            detect_terminal_state("Do you want to continue? [Y/n] ", 31, true),
            TerminalState::Confirm
        );
    }

    #[test]
    fn test_ssh_confirm_cursor_col_zero() {
        // P0 反例：SSH fingerprint 确认光标在行首
        assert_eq!(
            detect_terminal_state(
                "Are you sure you want to continue connecting (yes/no)? ",
                0,
                true
            ),
            TerminalState::Confirm
        );
    }

    // ── Editor ──
    #[test]
    fn test_vim_insert_mode() {
        let text = "line 1\nline 2\n~\n-- INSERT --                                        2,1";
        assert_eq!(detect_terminal_state(text, 5, false), TerminalState::Editor);
    }

    #[test]
    fn test_git_commit_opens_vim() {
        let text = "\n# Please enter the commit message\n~\n-- INSERT --                                        1,1";
        assert_eq!(detect_terminal_state(text, 1, false), TerminalState::Editor);
    }

    #[test]
    fn test_vim_new_file() {
        let text = "~\n~\n~\n~\n\"/tmp/test.txt\" [New]                                                                                                                    0,0-1         All";
        assert_eq!(detect_terminal_state(text, 0, true), TerminalState::Editor);
    }

    #[test]
    fn test_vim_tilde_lines() {
        let text = "some code here\n~\n~\n~\n\"file.py\"                                                                                                                       1,1           All";
        assert_eq!(detect_terminal_state(text, 0, true), TerminalState::Editor);
    }

    // ── Pager ──
    #[test]
    fn test_less_end() {
        assert_eq!(
            detect_terminal_state("some content\n(END)", 5, false),
            TerminalState::Pager
        );
    }

    // ── REPL ──
    #[test]
    fn test_python_repl() {
        assert_eq!(
            detect_terminal_state("Python 3.12.0\n>>> ", 4, true),
            TerminalState::Repl
        );
    }

    // ── Unknown ──
    #[test]
    fn test_empty() {
        assert_eq!(detect_terminal_state("", 0, true), TerminalState::Unknown);
    }

    #[test]
    fn test_cursor_invisible_no_marker() {
        assert_eq!(
            detect_terminal_state("some random output", 5, false),
            TerminalState::Running
        );
    }

    #[test]
    fn test_zsh_prompt() {
        // 主题化 zsh（agnoster 等）以 ➜ 开头，不以 $ # > % 结尾 —— 也应识别为 Ready
        assert_eq!(
            detect_terminal_state("➜  ~ git:(main)", 17, true),
            TerminalState::Ready
        );
    }

    #[test]
    fn test_zsh_powerlevel10k_prompt() {
        // powerlevel10k 单行 prompt 以 ❯ 开头
        assert_eq!(
            detect_terminal_state("❯ ~/code/clum", 14, true),
            TerminalState::Ready
        );
    }

    #[test]
    fn test_zsh_theme_marker_midline_is_not_prompt() {
        // ➜ 出现在行中部（命令输出内容）不应误判为提示符
        assert_eq!(
            detect_terminal_state("result ➜ 42", 12, true),
            TerminalState::Unknown
        );
    }

    #[test]
    fn test_zsh_theme_prompt_running_when_cursor_col_zero() {
        // cursor 二次验证对主题化 zsh 同样生效
        assert_eq!(
            detect_terminal_state("➜ ~ git:(main)", 0, true),
            TerminalState::Running
        );
    }

    #[test]
    fn test_htop_tui() {
        let text = "  PID USER      PR  NI    VIRT    RES    SHR S  %CPU  %MEM\n    1 root      20   0  168576  13248   8576 S   0.0   0.2";
        assert_eq!(
            detect_terminal_state(text, 0, false),
            TerminalState::Running
        );
    }

    #[test]
    fn test_command_output_with_dollar() {
        let text = "total $100\nprice is $50";
        // 最后一行 "price is $50" 不以 $ 结尾（以 "0" 结尾），所以是 Unknown
        assert_eq!(
            detect_terminal_state(text, 12, true),
            TerminalState::Unknown
        );
    }
}
