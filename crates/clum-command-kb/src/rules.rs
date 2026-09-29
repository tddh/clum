use anyhow::{Context, Result};
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
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let mut rs: RuleSet = toml::from_str(text).context("parse command rules TOML")?;
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
            } else if !rule.host_group.is_empty() {
                continue; // 规则限定了主机组，但当前无主机组 → 不命中
            }
            if !rule.caller_group.is_empty()
                && !caller_group
                    .map(|cg| rule.caller_group.iter().any(|g| g == cg))
                    .unwrap_or(false)
            {
                continue;
            }
            if let Some(pat) = &rule.argv_regex {
                match Regex::new(pat) {
                    Ok(re) if !re.is_match(&joined) => continue,
                    Ok(_) => {}
                    Err(_) => continue, // 非法正则 → 该规则视为不命中（规则文件加载时已告警）
                }
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
}
