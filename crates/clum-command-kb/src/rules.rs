use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::effect::Effect;

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct RuleSet {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub rule: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Rule {
    pub id: String,
    /// 数值小者先评估，命中即停
    #[serde(default)]
    pub priority: i32,
    /// 空 = 作用于所有工具
    #[serde(default)]
    pub tool: Vec<String>,
    /// 空 = 任意 program
    #[serde(default)]
    pub program: Vec<String>,
    /// 对「program + 空格连接的 argv」做正则匹配
    #[serde(default)]
    pub argv_regex: Option<String>,
    /// 加载时预编译；`argv_regex` 非法会让整个规则文件 fail-closed。
    /// 每次匹配重新 `Regex::new` 会让规则数线性放大单次裁决延迟（实测 200 条 ≈7ms/次）。
    #[serde(skip)]
    pub compiled_regex: Option<Regex>,
    /// 空 = 任意主机组
    #[serde(default)]
    pub host_group: Vec<String>,
    /// 空 = 任意调用者组
    #[serde(default)]
    pub caller_group: Vec<String>,
    pub action: Effect,
    #[serde(default)]
    pub reason: Option<String>,
}

impl RuleSet {
    /// 解析并**预编译**规则。任何结构性错误（非法正则、重复 id）都返回 Err——
    /// 规则文件解析失败必须 fail-closed，绝不静默丢弃一条规则。
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let mut rs: RuleSet = toml::from_str(text).context("parse command rules TOML")?;

        let mut seen = std::collections::HashSet::new();
        for rule in &mut rs.rule {
            if !seen.insert(rule.id.clone()) {
                bail!("duplicate rule id '{}' in command rules", rule.id);
            }
            if let Some(pat) = &rule.argv_regex {
                let re = Regex::new(pat)
                    .with_context(|| format!("rule '{}': invalid argv_regex {pat:?}", rule.id))?;
                rule.compiled_regex = Some(re);
            }
        }

        rs.rule.sort_by_key(|r| r.priority);
        Ok(rs)
    }

    /// 首个命中即返回（已按 priority 升序）。
    pub fn match_rule(
        &self,
        tool: &str,
        program: &str,
        args: &[String],
        caller_group: Option<&str>,
        host_group: Option<&str>,
    ) -> Option<&Rule> {
        let joined = if args.is_empty() {
            program.to_string()
        } else {
            format!("{program} {}", args.join(" "))
        };

        for rule in &self.rule {
            if !rule.tool.is_empty() && !rule.tool.iter().any(|t| t == tool) {
                continue;
            }
            if !rule.program.is_empty() && !rule.program.iter().any(|p| p == program) {
                continue;
            }
            if let Some(hg) = host_group {
                if !rule.host_group.is_empty() && !rule.host_group.iter().any(|g| g == hg) {
                    continue;
                }
            } else if !rule.host_group.is_empty() && rule.action == Effect::Allow {
                // Unknown host group: deny/ask rules still apply (strict), but a
                // group-scoped allow must not leak onto a host whose group could
                // not be resolved. Falling through to the KB is fail-closed.
                continue;
            }
            // 注意：host_group 规则在「当前主机组未知」时，deny/ask **仍然匹配**
            // （从严）。未知不等于不属于该组；静默跳过会让 prod 规则在组解析
            // 失败时失效。但 allow 例外（见上），否则 `host_group=["dev"] allow`
            // 会放行任何组信息缺失的主机。
            // caller_group 与 host_group 刻意不对称：host 组未知时 deny/ask
            // 规则仍然匹配（从严），而调用者组未知时跳过该规则。调用者是已
            // 认证主体，缺失值通常意味着「superadmin/无组」，把 prod 类规则
            // 套到所有无组调用者会误伤管理操作；host 组缺失则可能只是注册
            // 信息不全，从严更安全。
            if !rule.caller_group.is_empty()
                && !caller_group
                    .map(|cg| rule.caller_group.iter().any(|g| g == cg))
                    .unwrap_or(false)
            {
                continue;
            }
            if let Some(re) = &rule.compiled_regex {
                if !re.is_match(&joined) {
                    continue;
                }
            } else {
                // 只有 `from_toml_str` 会编译正则；若有人绕过它构造 RuleSet
                // （字段全 pub + Deserialize），argv_regex 会静默失效。debug 构建
                // 下立即暴露这个 footgun。
                debug_assert!(
                    rule.argv_regex.is_none(),
                    "rule '{}' has argv_regex but no compiled regex; construct RuleSet via from_toml_str",
                    rule.id
                );
            }
            return Some(rule);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = r#"
schema_version = 1
default_effect = "ask"

[[rule]]
id = "block-destructive-rm"
priority = 10
tool = ["exec", "batch_exec"]
program = ["rm", "mkfs.ext4", "dd"]
action = "deny"

[[rule]]
id = "prod-write-approval"
priority = 50
host_group = ["prod"]
argv_regex = "^(systemctl (restart|stop)|docker rm)"
action = "ask"
reason = "prod 环境变更需人工批准"
"#;

    fn set() -> RuleSet {
        RuleSet::from_toml_str(RULES).unwrap()
    }

    #[test]
    fn matches_program_and_tool() {
        let rs = set();
        let got = rs.match_rule("exec", "rm", &["-rf".into(), "/".into()], None, Some("dev"));
        assert_eq!(got.unwrap().action, Effect::Deny);
    }

    #[test]
    fn tool_scope_respected() {
        let rs = set();
        // send_keys 不在 tool 列表内 → 不命中 block-destructive-rm
        let got = rs.match_rule(
            "send_keys",
            "rm",
            &["-rf".into(), "/".into()],
            None,
            Some("dev"),
        );
        assert!(got.is_none());
    }

    #[test]
    fn priority_lower_wins() {
        let rs = RuleSet::from_toml_str(
            r#"
schema_version = 1
[[rule]]
id = "later"
priority = 90
program = ["rm"]
action = "allow"
[[rule]]
id = "earlier"
priority = 10
program = ["rm"]
action = "deny"
"#,
        )
        .unwrap();
        let got = rs.match_rule("exec", "rm", &[], None, None).unwrap();
        assert_eq!(got.id, "earlier");
    }

    #[test]
    fn host_group_scope_respected() {
        let rs = set();
        let args = vec!["restart".to_string(), "nginx".to_string()];
        assert!(rs
            .match_rule("exec", "systemctl", &args, None, Some("dev"))
            .is_none());
        assert!(rs
            .match_rule("exec", "systemctl", &args, None, Some("prod"))
            .is_some());
    }

    #[test]
    fn argv_regex_matches_joined_command() {
        let rs = set();
        let got = rs.match_rule(
            "exec",
            "docker",
            &["rm".into(), "container".into()],
            None,
            Some("prod"),
        );
        assert_eq!(got.unwrap().id, "prod-write-approval");
    }

    #[test]
    fn no_match_returns_none() {
        let rs = set();
        assert!(rs.match_rule("exec", "ls", &[], None, None).is_none());
    }

    /// 非法正则必须在加载期失败（fail-closed），不能静默丢弃一条规则。
    #[test]
    fn invalid_argv_regex_fails_loading() {
        let err = RuleSet::from_toml_str(
            r#"
schema_version = 1
[[rule]]
id = "bad-regex"
argv_regex = "^(unclosed"
action = "ask"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid argv_regex"), "got: {err}");
    }

    #[test]
    fn duplicate_rule_ids_fail_loading() {
        let err = RuleSet::from_toml_str(
            r#"
schema_version = 1
[[rule]]
id = "same"
action = "ask"
[[rule]]
id = "same"
action = "deny"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate rule id"), "got: {err}");
    }

    /// 主机组未知时，按主机组限定的规则仍然生效（从严），而不是静默跳过。
    #[test]
    fn unknown_host_group_still_matches_scoped_rule() {
        let rs = set();
        let args = vec!["restart".to_string(), "nginx".to_string()];
        let got = rs.match_rule("exec", "systemctl", &args, None, None);
        assert_eq!(got.map(|r| r.id.as_str()), Some("prod-write-approval"));
    }
}
