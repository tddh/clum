//! 绕过回归集：命令规则引擎的安全底线测试。
//!
//! 每条用例都来自真实生产事故或公开研究（Claude Code issue #4956、CVE-2025-66032、
//! arXiv 2606.15549 等）。**断言只允许 `>= Ask`** —— 一旦某条绕过路径得到 `Allow`，
//! 就意味着安全边界被击穿。修实现，绝不放宽断言。

use clum_command_kb::{evaluate, normalize, Effect, EvalContext, KnowledgeBase, RuleSet};

fn kb() -> KnowledgeBase {
    KnowledgeBase::builtin().expect("builtin knowledge base must load")
}

fn eval(cmd: &str) -> Effect {
    evaluate(
        &kb(),
        &RuleSet::default(),
        cmd,
        &EvalContext {
            tool: "exec".to_string(),
            caller_group: None,
            host_group: None,
        },
    )
    .unwrap()
    .effect
}

/// 任何形态的破坏性命令都不得被放行（至少 Ask）。
#[test]
fn bypass_attempts_never_allow_destructive() {
    let attempts = [
        ("r''m -rf /", "引号拆解"),
        ("\"rm\" \"-rf\" /", "引号包裹"),
        ("\\r\\m -rf /", "反斜杠转义"),
        ("ls && rm -rf /", "链式（Claude Code issue #4956）"),
        ("ls&&rm -rf /", "紧凑链式"),
        ("true; rm -rf /", "分号（带空格）"),
        ("true;rm -rf /", "分号（紧凑）"),
        ("ls\nrm -rf /", "换行分隔"),
        ("ls | xargs rm -rf", "管道 + xargs 包装"),
        ("sudo env VAR=1 /bin/rm -rf /", "包装器穿透 + 绝对路径"),
        ("/bin/rm -rf /", "绝对路径"),
        ("sh -c 'rm -rf /'", "sh -c"),
        ("bash -c 'rm -rf /'", "bash -c"),
        ("eval 'rm -rf /'", "eval 包装"),
        ("echo $(rm -rf /tmp/x)", "命令替换"),
        ("echo test`id`", "反引号替换"),
        ("cat <<EOF", "heredoc"),
        ("echo hi > /etc/passwd", "重定向"),
        ("dd if=/dev/zero of=/dev/sda", "设备写入"),
        ("mkfs.ext4 /dev/sdb", "格式化"),
        ("shred -u /dev/sda", "擦除"),
        ("chmod -R 777 /", "权限放开"),
        ("kubectl delete pods --all", "子命令 + 危险 flag"),
        ("kubectl delete ns kube-system", "子命令 + 高危 namespace"),
        ("systemctl poweroff", "overlay 中的 deny"),
        ("sudo systemctl stop nginx", "包装器 + overlay ask"),
        ("totally-unknown-cmd --danger", "未知命令"),
    ];

    for (cmd, why) in attempts {
        let effect = eval(cmd);
        assert!(
            effect >= Effect::Ask,
            "BYPASS: {why} —— `{cmd}` 被判为 {effect:?}，必须至少 Ask"
        );
    }
}

/// 只读命令不应打扰人（否则规则引擎会被运维关掉）。
#[test]
fn read_only_commands_do_not_prompt() {
    let read_only = [
        "ls -la",
        "cat /etc/hostname",
        "grep -n foo /tmp/f",
        "kubectl get pods",
        "kubectl describe pod x",
        "systemctl status nginx",
        "df -h",
        "du -sh /var/log",
        "journalctl -n 50",
        "git status",
        "git log --oneline",
    ];

    for cmd in read_only {
        assert_eq!(eval(cmd), Effect::Allow, "unexpected prompt for `{cmd}`");
    }
}

/// 制表符是**词分隔符，不是命令分隔符**（POSIX）。`ls\trm` 在 bash 中等价于
/// `ls rm`（一条命令，ls 的参数是 rm），所以只产生 1 个命令是**正确行为，不是绕过**。
#[test]
fn tab_is_word_separator_not_command_separator() {
    let out = normalize("ls\trm -rf /").expect("normalize");
    assert_eq!(out.commands.len(), 1, "tab 不应切分命令");
    assert_eq!(out.commands[0].program, "ls");
}

/// 连续/空分隔符不得产生空 program 段。
#[test]
fn repeated_separators_produce_no_empty_programs() {
    let out = normalize("ls && && cat /f").expect("normalize");
    assert!(out.commands.iter().all(|c| !c.program.is_empty()));
}

/// 复合命令取最严：只读 + 破坏性 → 整体按破坏性处理。
#[test]
fn compound_takes_strictest() {
    let d = evaluate(
        &kb(),
        &RuleSet::default(),
        "ls -la && systemctl poweroff",
        &EvalContext {
            tool: "exec".to_string(),
            caller_group: None,
            host_group: None,
        },
    )
    .unwrap();
    assert_eq!(d.commands.len(), 2, "应切分为两段");
    assert!(
        d.effect >= Effect::Ask,
        "复合命令必须取最严，得到 {:?}",
        d.effect
    );
}

/// 引号未闭合必须 fail-closed（解析失败 ≠ 放行）。
#[test]
fn unclosed_quote_fails_closed() {
    let d = evaluate(
        &kb(),
        &RuleSet::default(),
        "echo 'unclosed",
        &EvalContext {
            tool: "exec".to_string(),
            caller_group: None,
            host_group: None,
        },
    )
    .unwrap();
    assert!(d.effect >= Effect::Ask);
    assert_eq!(d.source, "parse-error");
}
