use crate::effect::Effect;
use crate::kb::KnowledgeBase;
use crate::normalize::{normalize, Hazard};
use crate::rules::RuleSet;

#[derive(Debug, Clone)]
pub struct EvalContext {
    pub tool: String,
    pub caller_group: Option<String>,
    pub host_group: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub effect: Effect,
    /// 决策来源：`rule:<id>` / `kb:<program>[ <sub>]` / `unknown` / `parse-error` / `empty`
    pub source: String,
    pub reason: String,
    /// 归一化后的命令（program + args）描述，审计用
    pub commands: Vec<String>,
    /// 全部段命中的危险结构（去重）
    pub hazards: Vec<Hazard>,
    /// 所有段都被显式标记为只读、且没有特权提升/危险结构。
    pub read_only: bool,
}

/// 裁决入口。永不返回 Err——解析失败以 fail-closed 方式降级为 Ask，
/// 因为「无法判断」绝不能等同于「放行」。
pub fn evaluate(
    kb: &KnowledgeBase,
    rules: &RuleSet,
    input: &str,
    ctx: &EvalContext,
) -> Result<Decision, std::convert::Infallible> {
    if input.trim().is_empty() {
        return Ok(Decision {
            effect: Effect::Ask,
            source: "empty".to_string(),
            reason: "empty command".to_string(),
            commands: Vec::new(),
            hazards: Vec::new(),
            read_only: false,
        });
    }

    let outcome = match normalize(input) {
        Ok(o) => o,
        Err(e) => {
            return Ok(Decision {
                effect: Effect::Ask,
                source: "parse-error".to_string(),
                reason: format!("command could not be parsed (fail-closed): {e}"),
                commands: vec![input.to_string()],
                hazards: Vec::new(),
                read_only: false,
            });
        }
    };

    if outcome.commands.is_empty() {
        return Ok(Decision {
            effect: Effect::Ask,
            source: "empty".to_string(),
            reason: "no executable command found".to_string(),
            commands: Vec::new(),
            hazards: Vec::new(),
            read_only: false,
        });
    }

    let mut aggregate = Effect::Allow;
    let mut worst_source = String::new();
    let mut worst_reason = String::new();
    let mut commands = Vec::new();
    let mut hazards: Vec<Hazard> = Vec::new();
    let mut aggregate_read_only = true;
    // `sudo`/`doas` elevate privileges; a read-only principal must not use
    // them even when the inner command is read-only.
    let privileged = has_privilege_elevation(input);

    for cmd in &outcome.commands {
        let mut seg = if let Some(rule) = rules.match_rule(
            &ctx.tool,
            &cmd.program,
            &cmd.args,
            ctx.caller_group.as_deref(),
            ctx.host_group.as_deref(),
        ) {
            Decision {
                effect: rule.action,
                source: format!("rule:{}", rule.id),
                reason: rule
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("matched rule '{}'", rule.id)),
                commands: Vec::new(),
                hazards: Vec::new(),
                read_only: false,
            }
        } else {
            let (effect, source, read_only) = kb.lookup_with_meta(&cmd.program, &cmd.args);
            Decision {
                effect,
                source,
                reason: format!("knowledge base classification for '{}'", cmd.program),
                commands: Vec::new(),
                hazards: Vec::new(),
                read_only,
            }
        };

        // 危险结构升格：任何 hazard 至少 Ask
        if !cmd.hazards.is_empty() {
            let escalated = seg.effect.max(Effect::Ask);
            if escalated != seg.effect {
                seg.reason = format!(
                    "{}; escalated to ask by structural hazard {:?}",
                    seg.reason, cmd.hazards
                );
            }
            seg.effect = escalated;
            seg.read_only = false;
        }

        aggregate_read_only &= seg.read_only;
        hazards.extend(cmd.hazards.iter().copied());
        commands.push(describe(cmd));

        if seg.effect > aggregate {
            aggregate = seg.effect;
            worst_source = seg.source;
            worst_reason = seg.reason;
        } else if worst_source.is_empty() {
            // 放行同样要可追溯：全 Allow 时聚合器不会被「更严的段」触发，
            // 若不在此留下来源，Decision.source 会是空串，审计与复盘无从知道命中了哪条。
            worst_source = seg.source;
            worst_reason = seg.reason;
        }
    }

    hazards.sort_by_key(|h| format!("{h:?}"));
    hazards.dedup();

    Ok(Decision {
        effect: aggregate,
        source: worst_source,
        reason: worst_reason,
        commands,
        hazards,
        read_only: aggregate_read_only && !privileged,
    })
}

/// Conservative privilege-elevation check. False positives only deny read-only
/// mode (fail-closed); the command still runs under `command:exec`.
fn has_privilege_elevation(input: &str) -> bool {
    input
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/'))
        .any(|token| {
            token
                .rsplit('/')
                .next()
                .is_some_and(|base| base == "sudo" || base == "doas")
        })
}

fn describe(cmd: &crate::normalize::SimpleCommand) -> String {
    if cmd.args.is_empty() {
        cmd.program.clone()
    } else {
        format!("{} {}", cmd.program, cmd.args.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::KnowledgeBase;
    use crate::rules::RuleSet;

    fn kb() -> KnowledgeBase {
        KnowledgeBase::from_toml_str(
            r#"
schema_version = 1
default_effect = "ask"
[[command]]
name = "ls"
effect = "allow"
[[command]]
name = "rm"
effect = "ask"
dangerous_flags = ["-rf"]
escalate_to = "deny"
"#,
        )
        .unwrap()
    }

    fn ctx(tool: &str) -> EvalContext {
        EvalContext {
            tool: tool.to_string(),
            caller_group: None,
            host_group: None,
        }
    }

    #[test]
    fn read_only_command_allowed() {
        let d = evaluate(&kb(), &RuleSet::default(), "ls -la /tmp", &ctx("exec")).unwrap();
        assert_eq!(d.effect, Effect::Allow);
    }

    #[test]
    fn dangerous_rm_denied() {
        let d = evaluate(&kb(), &RuleSet::default(), "rm -rf /", &ctx("exec")).unwrap();
        assert_eq!(d.effect, Effect::Deny);
        // source 形如 "kb:rm dangerous-flag(-rf)"——断言前缀，不锁死完整后缀
        assert!(d.source.starts_with("kb:rm"), "got: {}", d.source);
        assert!(d.source.contains("dangerous-flag"));
    }

    #[test]
    fn read_only_aggregates_all_segments_and_rejects_privilege() {
        let kb = KnowledgeBase::builtin().unwrap();
        let rules = RuleSet::default();
        let ctx = self::ctx("exec");

        let d = evaluate(&kb, &rules, "ls -la", &ctx).unwrap();
        assert!(d.read_only, "single allow + whitelist is read-only");

        let d = evaluate(&kb, &rules, "ls && echo x", &ctx).unwrap();
        assert!(!d.read_only, "unmarked allow is not read-only");

        let d = evaluate(&kb, &rules, "sudo ls", &ctx).unwrap();
        assert!(!d.read_only, "sudo must never be read-only");

        let d = evaluate(&kb, &rules, "ls > /tmp/x", &ctx).unwrap();
        assert!(!d.read_only, "redirect hazard must clear read-only");
    }

    #[test]
    fn compound_takes_strictest() {
        // ls(allow) + rm(deny) → 整体 deny（Claude Code issue #4956 的绕过场景）
        let d = evaluate(&kb(), &RuleSet::default(), "ls && rm -rf /", &ctx("exec")).unwrap();
        assert_eq!(d.effect, Effect::Deny);
        assert_eq!(d.commands.len(), 2);
    }

    #[test]
    fn unknown_program_asks_never_allows() {
        let d = evaluate(
            &kb(),
            &RuleSet::default(),
            "totally-new-tool --x",
            &ctx("exec"),
        )
        .unwrap();
        assert_eq!(d.effect, Effect::Ask);
    }

    #[test]
    fn substitution_hazard_escalates_to_ask() {
        // 即便 program 是 ls（allow），出现命令替换也必须升格
        let d = evaluate(
            &kb(),
            &RuleSet::default(),
            "ls $(rm -rf /tmp/x)",
            &ctx("exec"),
        )
        .unwrap();
        assert!(d.effect >= Effect::Ask);
        assert!(d
            .hazards
            .contains(&crate::normalize::Hazard::CommandSubstitution));
    }

    #[test]
    fn redirect_hazard_escalates_to_ask() {
        let d = evaluate(
            &kb(),
            &RuleSet::default(),
            "echo hi > /etc/passwd",
            &ctx("exec"),
        )
        .unwrap();
        assert!(d.effect >= Effect::Ask);
    }

    #[test]
    fn parse_error_is_fail_closed() {
        let d = evaluate(&kb(), &RuleSet::default(), "echo 'unclosed", &ctx("exec")).unwrap();
        assert_eq!(d.effect, Effect::Ask);
        assert_eq!(d.source, "parse-error");
    }

    #[test]
    fn user_rule_overrides_kb() {
        let rules = RuleSet::from_toml_str(
            r#"
schema_version = 1
[[rule]]
id = "deny-ls"
program = ["ls"]
action = "deny"
"#,
        )
        .unwrap();
        let d = evaluate(&kb(), &rules, "ls", &ctx("exec")).unwrap();
        assert_eq!(d.effect, Effect::Deny);
        assert_eq!(d.source, "rule:deny-ls");
    }

    #[test]
    fn empty_command_is_ask() {
        let d = evaluate(&kb(), &RuleSet::default(), "   ", &ctx("exec")).unwrap();
        assert_eq!(d.effect, Effect::Ask);
    }

    /// F-10：放行也必须可追溯。此前 `worst_source` 只在「更严的段出现」时才写入，
    /// 全 Allow 的命令得到空 source，审计与复盘无法知道命中了哪条知识库条目。
    #[test]
    fn allow_decision_records_source() {
        let d = evaluate(&kb(), &RuleSet::default(), "ls -la", &ctx("exec")).unwrap();
        assert_eq!(d.effect, Effect::Allow);
        assert!(!d.source.is_empty(), "放行也须留下命中来源，否则不可追溯");
        assert!(
            d.source.starts_with("kb:"),
            "source 应指向知识库条目，实际 {:?}",
            d.source
        );
    }
}
