use anyhow::{bail, Result};

/// 包装器命令：匹配前剥离，其后的参数按包装器语义跳过。
/// 与 Claude Code 的归一化集合对齐（timeout/time/nice/nohup/stdbuf）并补充 setsid/env/sudo/doas/xargs。
const WRAPPERS: &[&str] = &[
    "timeout", "time", "nice", "nohup", "stdbuf", "setsid", "env", "sudo", "doas", "xargs",
];

/// 包装器最大穿透深度：超过则视为危险（Codex is_dangerous_command 同款 fail-closed）
const MAX_WRAPPER_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hazard {
    Redirection,
    CommandSubstitution,
    Backtick,
    Heredoc,
    WrapperOverflow,
    /// TTY 控制字符（Ctrl-U/Ctrl-W/ESC 等）。`send_keys` 发送的是按键流而非
    /// shell 命令行：控制字符会触发行编辑/终端序列，绕过按 shell 语义做的切分。
    ControlChar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    pub program: String,
    pub args: Vec<String>,
    /// 归一化过程中剥离掉的包装器（审计留痕）
    pub wrappers: Vec<String>,
    /// 该段内命中的危险结构——任一存在时调用方必须至少 Ask
    pub hazards: Vec<Hazard>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizeOutcome {
    pub commands: Vec<SimpleCommand>,
    /// 原始输入（审计用）
    pub raw: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerKind {
    Sep,
    Hazard(Hazard),
}

#[derive(Debug, Clone, Copy)]
struct Marker {
    pos: usize,
    len: usize,
    kind: MarkerKind,
}

/// 扫描原始输入，返回未加引号区域的分隔符与危险结构标记。
fn scan_markers(input: &str) -> Vec<Marker> {
    let bytes = input.as_bytes();
    let mut markers = Vec::new();
    let (mut in_single, mut in_double) = (false, false);
    let mut i = 0usize;

    while i < bytes.len() {
        let c = bytes[i];

        // 非 ASCII 字节 = UTF-8 多字节字符的一部分。它不可能是任何分隔符或危险结构的
        // 首字节（那些全是 ASCII），必须**整字符**跳过：若按字节 `i += 1`，i 会落进字符
        // 内部，使下面 `&input[i..]` 在非 char boundary 上切片而 panic
        // （实测 `grep 错误 f`、`kubectl get pods -n 生产` 必崩）。
        // 本分支同时保证 i 始终停在 char boundary 上。
        if c >= 0x80 {
            i += input[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        }

        // TTY 语义：CR 与 LF 一样是 Enter；其他 C0 控制字符会触发 readline/终端
        // 行编辑（Ctrl-U 清行、Ctrl-W 删词、ESC 序列等）。`send_keys` 的载荷是
        // 按键流，shell 引号不会改变终端行为，因此这些字符在任何引号状态下都要识别。
        // 否则 `cat /etc/shadow\rrm -rf /` 会被当成一条 `cat` 命令判 Allow，
        // 而终端实际执行两行。
        if c == b'\r' {
            markers.push(Marker {
                pos: i,
                len: 1,
                kind: MarkerKind::Sep,
            });
            i += 1;
            continue;
        }
        if (c < 0x20 && c != b'\n' && c != b'\t') || c == 0x7f {
            markers.push(Marker {
                pos: i,
                len: 1,
                kind: MarkerKind::Hazard(Hazard::ControlChar),
            });
            i += 1;
            continue;
        }

        // 转义：跳过下一个**字符**（单引号内不生效）。
        // 同样不能按字节跳——`\€` 会让 i 落进 € 内部（i+1 必为 boundary：`\` 是 ASCII）。
        if c == b'\\' && !in_single {
            i += 1 + input[i + 1..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        if c == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if c == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if in_single {
            i += 1;
            continue;
        }

        let rest = &input[i..];

        // 反引号：双引号内仍然生效
        if c == b'`' {
            markers.push(Marker {
                pos: i,
                len: 1,
                kind: MarkerKind::Hazard(Hazard::Backtick),
            });
            i += 1;
            continue;
        }

        // 命令替换 $(...)：双引号内**仍然生效**（shell 语义），故不加 in_double 保护。
        if rest.starts_with("$(") {
            markers.push(Marker {
                pos: i,
                len: 2,
                kind: MarkerKind::Hazard(Hazard::CommandSubstitution),
            });
            i += 2;
            continue;
        }

        // 以下结构在双引号内均为**字面字符**，必须加 in_double 保护：
        // 进程替换 <() / >()、heredoc <<、重定向 > < >>。
        // 漏掉该保护会让 `echo "a > b"`、`printf "x->y"` 这类日常命令被误升为 ask，
        // 持续制造审批噪声（闸门会因此被运维关掉）。
        // 反引号已在上方单独处理：它与 $() 一样在双引号内生效，故不在此块内。
        if !in_double {
            if rest.starts_with("<(") || rest.starts_with(">(") {
                markers.push(Marker {
                    pos: i,
                    len: 2,
                    kind: MarkerKind::Hazard(Hazard::CommandSubstitution),
                });
                i += 2;
                continue;
            }
            if rest.starts_with("<<") {
                markers.push(Marker {
                    pos: i,
                    len: 2,
                    kind: MarkerKind::Hazard(Hazard::Heredoc),
                });
                i += 2;
                continue;
            }
            if c == b'>' || c == b'<' {
                let len = if rest.starts_with(">>") { 2 } else { 1 };
                // 跳过 fd 重定向尾部（>&1 / >&2），避免其 "&" 被误判为分隔符
                let mut advance = len;
                let tail = &rest[len..];
                if tail.starts_with('&') && tail[1..].starts_with(|ch: char| ch.is_ascii_digit()) {
                    advance = len + 2;
                }
                markers.push(Marker {
                    pos: i,
                    len: advance,
                    kind: MarkerKind::Hazard(Hazard::Redirection),
                });
                i += advance;
                continue;
            }
        }

        // 分隔符（双引号内不生效）
        if !in_double {
            const SEPS: &[(&str, usize)] = &[
                ("&&", 2),
                ("||", 2),
                (";", 1),
                ("|", 1),
                ("&", 1),
                ("\n", 1),
            ];
            let mut matched = None;
            for (s, l) in SEPS {
                if rest.starts_with(s) {
                    matched = Some(*l);
                    break;
                }
            }
            if let Some(l) = matched {
                markers.push(Marker {
                    pos: i,
                    len: l,
                    kind: MarkerKind::Sep,
                });
                i += l;
                continue;
            }
        }

        i += 1;
    }
    markers
}

/// 剥离包装器与环境变量赋值前缀，返回 (program, args, wrappers)。
fn strip_wrappers(words: &[String]) -> Result<(String, Vec<String>, Vec<String>)> {
    let mut idx = 0usize;
    let mut wrappers = Vec::new();

    loop {
        if wrappers.len() >= MAX_WRAPPER_DEPTH {
            bail!("wrapper nesting exceeds {MAX_WRAPPER_DEPTH}");
        }
        let Some(head) = words.get(idx) else {
            bail!("empty command after stripping wrappers");
        };

        // env / VAR=val 前缀
        if head.contains('=') && !head.starts_with('-') {
            idx += 1;
            continue;
        }
        match head.as_str() {
            "env" => {
                wrappers.push(head.clone());
                idx += 1;
                continue;
            }
            "sudo" | "doas" => {
                wrappers.push(head.clone());
                idx += 1;
                // 跳过 sudo/doas 的选项与其参数
                while let Some(w) = words.get(idx) {
                    if w.starts_with('-') {
                        // sudo 的 `-h` 是 help（**不带参数**），不得列入取值选项：
                        // 否则会吞掉后续真实命令——`sudo -h rm -rf /` 会把 `rm` 吃掉，
                        // 使 program 退化为 basename("/") = ""。
                        let takes_value = matches!(
                            w.as_str(),
                            "-u" | "-g" | "-p" | "-C" | "-r" | "-t" | "-U" | "-D" | "-R" | "-T"
                        );
                        idx += 1;
                        if takes_value && words.get(idx).is_some() {
                            idx += 1;
                        }
                        continue;
                    }
                    break;
                }
                continue;
            }
            w if WRAPPERS.contains(&w) => {
                wrappers.push(head.clone());
                idx += 1;
                // 跳过该包装器的选项与数值参数（保守：只跳过一眼可判定的）
                while let Some(w) = words.get(idx) {
                    // `!w.is_empty()` 必需：`all` 对空迭代器返回 true，
                    // 否则空词（`timeout 30 "" rm`）会被当成数值参数跳过。
                    if w.starts_with('-')
                        || (!w.is_empty() && w.chars().all(|ch| ch.is_ascii_digit()))
                    {
                        idx += 1;
                        continue;
                    }
                    break;
                }
                continue;
            }
            _ => {}
        }
        break;
    }

    let program = words
        .get(idx)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no program found"))?;
    let args = words[idx + 1..].to_vec();
    Ok((program, args, wrappers))
}

/// 程序名归一：取路径 basename 并去掉一层引号（shell-words 已处理引号，此处兜底）。
fn program_basename(program: &str) -> String {
    let p = program.trim_matches('\'').trim_matches('"');
    p.rsplit('/').next().unwrap_or(p).to_string()
}

/// 归一化入口。引号未闭合返回 `Err`——调用方必须 fail-closed。
pub fn normalize(input: &str) -> Result<NormalizeOutcome> {
    let markers = scan_markers(input);

    // 按分隔符切出各段的字节区间
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for m in markers.iter().filter(|m| matches!(m.kind, MarkerKind::Sep)) {
        if m.pos > start {
            ranges.push((start, m.pos));
        }
        start = m.pos + m.len;
    }
    if start < input.len() {
        ranges.push((start, input.len()));
    }

    let mut commands = Vec::new();
    for (s, e) in ranges {
        let text = &input[s..e];
        let words = match shell_words::split(text) {
            Ok(w) => w,
            Err(err) => bail!("shell parse failed: {err}"),
        };
        if words.is_empty() {
            continue;
        }

        let mut hazards: Vec<Hazard> = markers
            .iter()
            .filter(|m| m.pos >= s && m.pos < e)
            .filter_map(|m| match m.kind {
                MarkerKind::Hazard(h) => Some(h),
                MarkerKind::Sep => None,
            })
            .collect();
        hazards.dedup();

        let (program, args, wrappers) = match strip_wrappers(&words) {
            Ok(v) => v,
            Err(_) => {
                // 包装器过深或剥离后为空 → 记 WrapperOverflow 并以原始首词为 program
                hazards.push(Hazard::WrapperOverflow);
                (words[0].clone(), words[1..].to_vec(), Vec::new())
            }
        };

        let program = program_basename(&program);
        if program.is_empty() {
            // 空 program（`/`、空词 `""` 等）：shell 本身无法执行。若无危险结构，
            // 丢弃该段以守住「不得产生空 program 段」不变量。
            //
            // 但重定向/命令替换在**命令查找之前**就已生效（`"" > /etc/passwd`
            // 会先截断文件、`"" $(rm -rf /tmp)` 会先执行替换）。若把带 hazard
            // 的段一并丢弃，`ls && "" > /etc/passwd` 会被聚合为 Allow——这是
            // 本段丢弃逻辑引入的绕过。保留一个 KB 不可能命中的哨兵段，让它
            // 走 unknown → Ask，同时把 hazard 带进审计。
            if hazards.is_empty() {
                continue;
            }
            commands.push(SimpleCommand {
                program: "__clum_empty_program__".to_string(),
                args,
                wrappers,
                hazards,
            });
            continue;
        }

        commands.push(SimpleCommand {
            program,
            args,
            wrappers,
            hazards,
        });
    }

    Ok(NormalizeOutcome {
        commands,
        raw: input.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_spaced_operators() {
        let out = normalize("cmd1 && cmd2 ; cmd3 | cmd4").unwrap();
        assert_eq!(
            out.commands
                .iter()
                .map(|c| c.program.clone())
                .collect::<Vec<_>>(),
            vec!["cmd1", "cmd2", "cmd3", "cmd4"]
        );
    }

    #[test]
    fn splits_compact_operators() {
        // 关键用例：无空格紧凑形态（shell-words 单独会把 "ls&&rm" 当一个词）
        let out = normalize("ls&&rm -rf /").unwrap();
        assert_eq!(
            out.commands
                .iter()
                .map(|c| c.program.clone())
                .collect::<Vec<_>>(),
            vec!["ls", "rm"]
        );
        assert_eq!(out.commands[1].args, vec!["-rf", "/"]);
    }

    #[test]
    fn separates_env_prefix_and_wrapper() {
        let out = normalize("sudo -u app env VAR=1 nice timeout 30 rm -rf /tmp/x").unwrap();
        assert_eq!(out.commands.len(), 1);
        assert_eq!(out.commands[0].program, "rm");
        assert_eq!(out.commands[0].args, vec!["-rf", "/tmp/x"]);
        assert!(out.commands[0].wrappers.contains(&"sudo".to_string()));
    }

    #[test]
    fn redirect_is_hazard_and_not_operator_split() {
        let out = normalize("echo hi > /etc/passwd").unwrap();
        assert_eq!(out.commands.len(), 1);
        assert_eq!(out.commands[0].program, "echo");
        assert!(out.commands[0].hazards.contains(&Hazard::Redirection));
    }

    #[test]
    fn fd_redirect_not_split_as_separator() {
        let out = normalize("cmd 2>&1").unwrap();
        assert_eq!(out.commands.len(), 1);
        assert_eq!(out.commands[0].program, "cmd");
    }

    #[test]
    fn command_substitution_is_hazard() {
        for input in ["echo $(whoami)", "echo `date`", "rm -rf /$(echo tmp)"] {
            let out = normalize(input).unwrap();
            let has = out.commands.iter().any(|c| {
                c.hazards
                    .iter()
                    .any(|h| matches!(h, Hazard::CommandSubstitution | Hazard::Backtick))
            });
            assert!(has, "expected substitution hazard for {input}");
        }
    }

    #[test]
    fn single_quotes_are_inert() {
        // 单引号内的算子和替换都不算危险
        let out = normalize("echo 'a && b' 'x|y' '$(no)'").unwrap();
        assert_eq!(out.commands.len(), 1);
        assert!(out.commands[0].hazards.is_empty());
        assert_eq!(out.commands[0].args, vec!["a && b", "x|y", "$(no)"]);
    }

    #[test]
    fn double_quotes_still_detect_substitution() {
        let out = normalize("echo \"$(whoami)\"").unwrap();
        assert!(out.commands[0]
            .hazards
            .iter()
            .any(|h| matches!(h, Hazard::CommandSubstitution)));
    }

    #[test]
    fn quote_obfuscation_normalized_to_real_program() {
        // r''m / "rm" / \r\m 都应归一为 rm（这几条是绕过测试的核心）
        for input in ["r''m -rf /", "\"rm\" -rf /", "\\r\\m -rf /"] {
            let out = normalize(input).unwrap();
            assert_eq!(out.commands[0].program, "rm", "input: {input}");
        }
    }

    #[test]
    fn unclosed_quote_is_error() {
        assert!(normalize("echo 'unclosed").is_err());
        assert!(normalize("echo \"unclosed").is_err());
    }

    #[test]
    fn empty_and_whitespace_inputs() {
        assert!(normalize("").unwrap().commands.is_empty());
        assert!(normalize("   ").unwrap().commands.is_empty());
    }

    #[test]
    fn tail_segment_after_operator() {
        let out = normalize("ls &&").unwrap();
        assert_eq!(out.commands.len(), 1);
        assert_eq!(out.commands[0].program, "ls");
    }

    /// 回归：UTF-8 多字节字符不得导致 panic。
    ///
    /// 主循环按**字节**推进，若把多字节字符的首字节当单字节处理（末尾 `i += 1`），
    /// 或转义分支按字节跳（`i += 2`），`i` 会落进字符内部，后续 `&input[i..]`
    /// 在非 char boundary 切片 → panic。中文运维命令几乎必然触发（实测
    /// `grep 错误 /var/log/app.log`、`kubectl get pods -n 生产` 均崩）。
    /// panic 绕过了 `evaluate()`「永不返回 Err」的契约。
    #[test]
    fn multibyte_characters_do_not_panic() {
        for input in [
            "echo 你好",
            "cat /var/log/日志.txt",
            "ls # 列目录",
            "echo 🚀",
            "café --check",
            "grep 错误 /var/log/app.log",
            "kubectl get pods -n 生产",
            "echo \\€", // 反斜杠 + 多字节：转义分支的同源缺陷
        ] {
            let out = normalize(input);
            assert!(out.is_ok(), "不应 panic 或报错，input={input:?}");
        }
    }

    /// 不 panic 只是底线：多字节字符还不得被误当作分隔符/危险结构，
    /// 且 program 与 args 的切分必须与 ASCII 场景一致。
    #[test]
    fn multibyte_arguments_parse_correctly() {
        let out = normalize("grep 错误 /var/log/app.log").unwrap();
        assert_eq!(out.commands.len(), 1, "中文参数不得被切成多段");
        assert_eq!(out.commands[0].program, "grep");
        assert_eq!(out.commands[0].args, vec!["错误", "/var/log/app.log"]);
        assert!(
            out.commands[0].hazards.is_empty(),
            "多字节字符不得被误判为危险结构"
        );

        // 多字节字符与真实分隔符共存时，仍须正确切分
        let out = normalize("grep 错误 f && echo 完成").unwrap();
        assert_eq!(out.commands.len(), 2);
        assert_eq!(out.commands[0].program, "grep");
        assert_eq!(out.commands[1].program, "echo");
        assert_eq!(out.commands[1].args, vec!["完成"]);
    }

    /// shell 语义：双引号内的 `>` `<` `<<` 是**字面字符**，不产生重定向/heredoc。
    /// 只有 `$()` 与反引号在双引号内仍然生效。
    /// 误判方向虽偏严，但 `echo "a > b"` / `printf "x->y"` 属日常命令，持续制造审批噪声
    /// 会命中「闸门因每条命令都要批准被运维关掉」的自毁路径（见 kb.rs 覆盖度守卫注释）。
    #[test]
    fn double_quoted_redirect_chars_are_literal() {
        for input in [
            "echo \"a > b\"",
            "printf \"%s\\n\" \"x->y\"",
            "echo \"usage: cmd <file>\"",
            "grep \"a << b\" f",
        ] {
            let out = normalize(input).unwrap();
            assert!(
                out.commands[0].hazards.is_empty(),
                "双引号内的 >/</<< 不应判为危险结构：{input:?} → {:?}",
                out.commands[0].hazards
            );
        }

        // 反向守卫 1：双引号内的命令替换仍须检测（shell 中确实生效）
        let out = normalize("echo \"$(whoami)\"").unwrap();
        assert!(
            out.commands[0]
                .hazards
                .iter()
                .any(|h| matches!(h, Hazard::CommandSubstitution)),
            "双引号内的 $() 仍须判为命令替换"
        );

        // 反向守卫 2：引号外的真实重定向仍须检测
        let out = normalize("echo hi > /etc/passwd").unwrap();
        assert!(out.commands[0].hazards.contains(&Hazard::Redirection));

        // 反向守卫 3：引号外的 heredoc 仍须检测
        let out = normalize("cat <<EOF").unwrap();
        assert!(out.commands[0].hazards.contains(&Hazard::Heredoc));
    }

    /// sudo 的 `-h` 是 help（不带参数），不得被当作取值选项而吞掉后续真实命令。
    #[test]
    fn sudo_help_flag_does_not_swallow_command() {
        let out = normalize("sudo -h rm -rf /").unwrap();
        assert_eq!(out.commands[0].program, "rm", "`rm` 不得被 `-h` 吞掉");
        assert_eq!(out.commands[0].args, vec!["-rf", "/"]);
        assert_eq!(out.commands[0].wrappers, vec!["sudo".to_string()]);
    }

    /// `send_keys` 的载荷是按键流：CR 是 Enter、Ctrl-U/Ctrl-W/ESC 会触发终端行编辑。
    /// 这些字符必须在裁决中按 TTY 语义处理，否则可整类绕过。
    #[test]
    fn carriage_return_splits_like_enter() {
        let out = normalize("cat /etc/shadow\rrm -rf /tmp").unwrap();
        assert_eq!(
            out.commands
                .iter()
                .map(|c| c.program.as_str())
                .collect::<Vec<_>>(),
            vec!["cat", "rm"],
            "CR must split commands (TTY treats it as Enter)"
        );
    }

    #[test]
    fn tty_control_characters_are_hazards() {
        for input in [
            "cat\x15rm -rf /",   // Ctrl-U (kill line)
            "cat\x17rm -rf /",   // Ctrl-W (delete word)
            "cat\x1b[Arm -rf /", // ESC sequence
            "echo\x00x",         // NUL
        ] {
            let out = normalize(input).unwrap();
            let hazards: Vec<Hazard> = out
                .commands
                .iter()
                .flat_map(|c| c.hazards.clone())
                .collect();
            assert!(
                hazards.contains(&Hazard::ControlChar),
                "control char must be a hazard: {input:?} -> {hazards:?}"
            );
        }
    }

    /// 空 program 段若携带 hazard（重定向/命令替换），不得因丢段而让整体 Allow。
    /// 重定向/替换在命令查找之前生效：`"" > f` 会先截断 f，`"" $(cmd)` 会先执行 cmd。
    #[test]
    fn empty_program_with_hazard_still_asks() {
        use crate::effect::Effect;
        use crate::evaluate::{evaluate, EvalContext};
        use crate::kb::KnowledgeBase;
        use crate::rules::RuleSet;

        let kb = KnowledgeBase::builtin().unwrap();
        let rules = RuleSet::default();
        let ctx = EvalContext {
            tool: "exec".to_string(),
            caller_group: None,
            host_group: None,
        };

        for input in [
            "ls && \"\" $(rm -rf /tmp)",
            "ls; \"\" > /etc/passwd",
            "true && '' > /etc/passwd",
        ] {
            let out = normalize(input).unwrap();
            assert!(
                out.commands.iter().any(|c| !c.hazards.is_empty()),
                "hazard must survive empty-program dropping: {input:?}"
            );
            let d = evaluate(&kb, &rules, input, &ctx).unwrap();
            assert!(
                d.effect >= Effect::Ask,
                "empty program with hazard must not allow: {input:?} -> {:?}",
                d.effect
            );
        }
        // 无 hazard 的空段仍应被安全丢弃（不产生空 program）。
        let out = normalize("ls && \"\"").unwrap();
        assert!(out.commands.iter().all(|c| !c.program.is_empty()));
    }

    /// 不变量：任何输入都不得产生空 `program` 段。
    /// `tests/e2e_bypass.rs` 已有同类断言，但只覆盖 `ls && && cat /f`，覆盖不到下列路径。
    #[test]
    fn no_empty_program_segments() {
        for input in [
            "sudo -h rm -rf /",   // -h 误判会让 program 变成 basename("/") = ""
            "sudo -h",            // 剥离包装器后无词
            "ls && && cat /f",    // 连续分隔符
            "timeout 30 \"\" rm", // 空词被 wrapper 的数值判定吞掉
            "/ rm",               // basename("/") 为空
        ] {
            let out = normalize(input).unwrap();
            assert!(
                out.commands.iter().all(|c| !c.program.is_empty()),
                "不得产生空 program 段：{input:?} → {:?}",
                out.commands
                    .iter()
                    .map(|c| (&c.program, &c.args))
                    .collect::<Vec<_>>()
            );
        }
    }
}
