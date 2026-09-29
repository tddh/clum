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

        // 转义：跳过下一个字节（单引号内不生效）
        if c == b'\\' && !in_single {
            i += 2;
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

        // 危险结构（命令替换 / 进程替换 / heredoc / 重定向）
        if rest.starts_with("$(") || rest.starts_with("<(") || rest.starts_with(">(") {
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
                        let takes_value = matches!(
                            w.as_str(),
                            "-u" | "-g" | "-p" | "-C" | "-h" | "-r" | "-t" | "-U"
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
                    if w.starts_with('-') || w.chars().all(|ch| ch.is_ascii_digit()) {
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

        commands.push(SimpleCommand {
            program: program_basename(&program),
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
}
