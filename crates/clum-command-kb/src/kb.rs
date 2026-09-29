use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::effect::Effect;

#[derive(Debug, Deserialize, Serialize)]
pub struct KbFile {
    pub schema_version: u32,
    #[serde(default)]
    pub default_effect: Effect,
    #[serde(default)]
    pub command: Vec<CommandEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommandEntry {
    pub name: String,
    /// 本体默认动作（子命令未列出时）
    #[serde(default)]
    pub effect: Option<Effect>,
    /// 子命令 token 数（OpenCode arity 机制），纯文档/校验用途
    #[serde(default)]
    pub arity: Option<u8>,
    #[serde(default)]
    pub subcommand: Vec<SubcommandEntry>,
    #[serde(default)]
    pub dangerous_flags: Vec<String>,
    /// 命中 dangerous_flags 时升级到的动作（默认 Ask）
    #[serde(default)]
    pub escalate_to: Option<Effect>,
    /// 数据来源（provenance，审计与合规）
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SubcommandEntry {
    /// 子命令 token 序列，如 ["get"] / ["rollout", "restart"]
    pub name: Vec<String>,
    pub effect: Effect,
    #[serde(default)]
    pub dangerous_flags: Vec<String>,
    #[serde(default)]
    pub source: Option<String>,
}

pub struct KnowledgeBase {
    commands: HashMap<String, CommandEntry>,
    default_effect: Effect,
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

impl KnowledgeBase {
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let file: KbFile = toml::from_str(text).context("parse knowledge base TOML")?;
        let mut commands = HashMap::new();
        for entry in file.command {
            commands.insert(entry.name.clone(), entry);
        }
        Ok(Self {
            commands,
            default_effect: file.default_effect,
        })
    }

    /// 内置知识库（编译期嵌入）。
    ///
    /// 三层，后者按同名整条覆盖前者：
    ///   1. `generated.toml` —— 由 `scripts/kb-fuse-safe-chains.py` 批量生成（约 1,600 条，最粗）
    ///   2. `commands.toml`  —— 人工 vet 过的核心条目（可收紧 generated 的判定）
    ///   3. `overlay.toml`   —— 本地运维 CLI 增量（优先级最高）
    pub fn builtin() -> Result<Self> {
        let mut kb = Self::from_toml_str(include_str!("../data/generated.toml"))?;
        kb.merge_overlay_str(include_str!("../data/commands.toml"))?;
        kb.merge_overlay_str(include_str!("../data/overlay.toml"))?;
        Ok(kb)
    }

    /// 叠加 overlay：同名命令整条替换（overlay 优先）。
    pub fn merge_overlay_str(&mut self, text: &str) -> Result<()> {
        let file: KbFile = toml::from_str(text).context("parse knowledge base overlay")?;
        for entry in file.command {
            self.commands.insert(entry.name.clone(), entry);
        }
        Ok(())
    }

    pub fn default_effect(&self) -> Effect {
        self.default_effect
    }

    /// 查询：返回 (Effect, source 描述)。`args` 为 program 之后的参数。
    pub fn lookup(&self, program: &str, args: &[String]) -> (Effect, String) {
        let Some(entry) = self.commands.get(basename(program)) else {
            return (self.default_effect, "unknown".to_string());
        };

        // 子命令匹配，取最长命中。
        // `name` 既可表示多 token 子命令序列（args 以其为前缀，如 ["rollout", "restart"]），
        // 也可表示一组同义子命令（任一元素等于 args[0]，如 ["get", "logs", "describe"]）。
        let mut best: Option<(&SubcommandEntry, usize)> = None;
        for sub in &entry.subcommand {
            let matched_len = if args.starts_with(sub.name.as_slice()) {
                sub.name.len()
            } else if args.first().is_some_and(|a| sub.name.contains(a)) {
                1
            } else {
                continue;
            };
            if best.map(|(_, l)| matched_len > l).unwrap_or(true) {
                best = Some((sub, matched_len));
            }
        }

        let (mut effect, mut source, sub_flags) = match best {
            Some((sub, _)) => (
                sub.effect,
                format!("kb:{} {}", entry.name, sub.name.join(" ")),
                sub.dangerous_flags.as_slice(),
            ),
            None => (
                entry.effect.unwrap_or(self.default_effect),
                format!("kb:{}", entry.name),
                &[][..],
            ),
        };

        // dangerous_flags 升级（本体与命中子命令的 flag 都算），取最严。
        // 匹配三种形态：精确（`--force`）、`--flag=value`、以及**短选项簇**
        // （声明 `-i` 命中 `-in`；声明 `-x` 命中 `-xzf`）——后者是 safe-chains 的
        // 命令级 level 丢失 flag 粒度后必须补的防线（如 `tar -czf` 会写盘）。
        let matches_flag = |declared: &[String], arg: &String| {
            declared.iter().any(|f| {
                f == arg
                    || arg.starts_with(&format!("{f}="))
                    || (f.len() == 2
                        && f.starts_with('-')
                        && arg.starts_with('-')
                        && !arg.starts_with("--")
                        && arg[1..].contains(&f[1..]))
            })
        };
        let hit = args
            .iter()
            .find(|a| matches_flag(&entry.dangerous_flags, a) || matches_flag(sub_flags, a));
        if let Some(flag) = hit {
            effect = effect.max(entry.escalate_to.unwrap_or(Effect::Ask));
            source = format!("{source} dangerous-flag({flag})");
        }

        (effect, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
schema_version = 1
default_effect = "ask"

[[command]]
name = "kubectl"
effect = "ask"
arity = 2
source = "prodagent(apache-2.0)"
  [[command.subcommand]]
  name = ["get", "logs", "describe"]
  effect = "allow"
  [[command.subcommand]]
  name = ["delete", "drain", "exec"]
  effect = "ask"

[[command]]
name = "rm"
effect = "ask"
dangerous_flags = ["-f", "--force", "-rf", "-fr"]
escalate_to = "deny"
"#;

    #[test]
    fn subcommand_lookup_hits_subcommand_effect() {
        let kb = KnowledgeBase::from_toml_str(SAMPLE).unwrap();
        let (e, src) = kb.lookup("kubectl", &["get".into(), "pods".into()]);
        assert_eq!(e, Effect::Allow);
        assert!(src.contains("kubectl get"));

        let (e, _) = kb.lookup("kubectl", &["delete".into(), "pod".into(), "x".into()]);
        assert_eq!(e, Effect::Ask);
    }

    #[test]
    fn unmatched_subcommand_falls_back_to_command_effect() {
        let kb = KnowledgeBase::from_toml_str(SAMPLE).unwrap();
        let (e, _) = kb.lookup("kubectl", &["someNewSub".into()]);
        assert_eq!(e, Effect::Ask);
    }

    #[test]
    fn unknown_program_uses_default_effect() {
        let kb = KnowledgeBase::from_toml_str(SAMPLE).unwrap();
        let (e, src) = kb.lookup("totally-unknown", &[]);
        assert_eq!(e, Effect::Ask);
        assert_eq!(src, "unknown");
    }

    #[test]
    fn dangerous_flag_escalates() {
        let kb = KnowledgeBase::from_toml_str(SAMPLE).unwrap();
        let (e, src) = kb.lookup("rm", &["-rf".into(), "/".into()]);
        assert_eq!(e, Effect::Deny);
        assert!(src.contains("dangerous-flag"));
    }

    #[test]
    fn safe_flags_keep_base_effect() {
        let kb = KnowledgeBase::from_toml_str(SAMPLE).unwrap();
        let (e, _) = kb.lookup("rm", &["/tmp/x".into()]);
        assert_eq!(e, Effect::Ask);
    }

    #[test]
    fn overlay_overrides_builtin() {
        let mut kb = KnowledgeBase::from_toml_str(SAMPLE).unwrap();
        let overlay = r#"
schema_version = 1
[[command]]
name = "kubectl"
effect = "deny"
"#;
        kb.merge_overlay_str(overlay).unwrap();
        let (e, _) = kb.lookup("kubectl", &["get".into()]);
        assert_eq!(e, Effect::Deny, "overlay 必须覆盖内置");
    }

    #[test]
    fn program_basename_matches_path_form() {
        let kb = KnowledgeBase::from_toml_str(SAMPLE).unwrap();
        let (e, _) = kb.lookup("/usr/bin/kubectl", &["get".into()]);
        assert_eq!(e, Effect::Allow);
    }

    #[test]
    fn builtin_kb_loads_and_has_expected_coverage() {
        let kb = KnowledgeBase::builtin().unwrap();
        // 只读命令放行
        assert_eq!(kb.lookup("cat", &[]).0, Effect::Allow);
        assert_eq!(
            kb.lookup(
                "grep",
                &["-n", "x"]
                    .into_iter()
                    .map(String::from)
                    .collect::<Vec<_>>()
            )
            .0,
            Effect::Allow
        );
        assert_eq!(kb.lookup("ls", &["-la".into()]).0, Effect::Allow);
        // 危险命令不 Allow
        assert!(kb.lookup("rm", &["/tmp/x".into()]).0 >= Effect::Ask);
        assert_eq!(kb.lookup("rm", &["-rf".into(), "/".into()]).0, Effect::Deny);
        assert!(kb.lookup("dd", &[]).0 >= Effect::Ask);
        assert!(kb.lookup("mkfs.ext4", &[]).0 >= Effect::Ask);
        // 运维 CLI 子命令粒度
        assert_eq!(
            kb.lookup("kubectl", &["get".into(), "pods".into()]).0,
            Effect::Allow
        );
        assert!(
            kb.lookup("kubectl", &["delete".into(), "pod".into(), "x".into()])
                .0
                >= Effect::Ask
        );
        assert!(kb.lookup("kubectl", &["drain".into(), "node1".into()]).0 >= Effect::Ask);
        assert_eq!(
            kb.lookup("systemctl", &["status".into(), "nginx".into()]).0,
            Effect::Allow
        );
        assert!(kb.lookup("systemctl", &["stop".into(), "nginx".into()]).0 >= Effect::Ask);
    }

    #[test]
    fn every_builtin_entry_has_source_provenance() {
        let kb = KnowledgeBase::from_toml_str(include_str!("../data/commands.toml")).unwrap();
        for (name, entry) in &kb.commands {
            assert!(
                entry.source.is_some(),
                "命令 '{name}' 缺少 source（provenance 必填，用于合规审计）"
            );
        }
    }

    #[test]
    fn overlay_takes_priority_for_ops_cli() {
        let kb = KnowledgeBase::builtin().unwrap();
        // overlay 中标注的运维命令必须生效（Task 4 Step 2 会写入）
        assert!(kb.lookup("journalctl", &["--vacuum-size=500M".into()]).0 >= Effect::Ask);
    }

    #[test]
    fn fused_data_covers_common_commands() {
        let kb = KnowledgeBase::builtin().unwrap();
        // 融合数据存在的意义：常见只读命令必须放行，否则闸门会因噪声被运维关掉
        for cmd in ["ps", "top", "netstat", "ss", "du", "df", "awk", "sort"] {
            assert_eq!(
                kb.lookup(cmd, &[]).0,
                Effect::Allow,
                "`{cmd}` 应放行（融合数据/手工收紧）"
            );
        }
        for cmd in ["rsync", "find", "dd", "mkfs.ext4", "chmod"] {
            assert!(kb.lookup(cmd, &[]).0 >= Effect::Ask, "`{cmd}` 必须至少 ask");
        }
    }

    #[test]
    fn safe_read_commands_escalate_on_write_flags() {
        let kb = KnowledgeBase::builtin().unwrap();
        // safe-chains 的命令级 level 丢失 flag 粒度 → 必须靠 dangerous_flags 升格；
        // 短选项簇形态（-xzf / -in）也要拦住，否则等于没拦。
        assert_eq!(
            kb.lookup("tar", &["-tzf".to_string(), "x.tar.gz".to_string()])
                .0,
            Effect::Allow,
            "tar 列表是只读，应放行"
        );
        for write_flag in ["-czf", "-xzf", "--delete"] {
            assert!(
                kb.lookup("tar", &[write_flag.to_string(), "x".to_string()])
                    .0
                    >= Effect::Ask,
                "tar {write_flag} 会写盘，必须至少 ask"
            );
        }
        assert_eq!(
            kb.lookup("sed", &["-n".to_string(), "1p".to_string()]).0,
            Effect::Allow,
            "sed -n 是只读，应放行"
        );
        for write_flag in ["-i", "-in", "--in-place"] {
            assert!(
                kb.lookup("sed", &[write_flag.to_string(), "s/a/b/".to_string()])
                    .0
                    >= Effect::Ask,
                "sed {write_flag} 会原地改写，必须至少 ask"
            );
        }
    }

    #[test]
    fn fused_knowledge_base_is_large_enough() {
        // 覆盖度守卫：知识库若退回几十条，闸门会因为「每条命令都要批准」被关掉
        let count = KnowledgeBase::from_toml_str(include_str!("../data/generated.toml"))
            .unwrap()
            .commands
            .len();
        assert!(count >= 1500, "融合数据不应少于 1500 条，实际 {count}");
    }
}
