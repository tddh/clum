use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::effect::Effect;

#[derive(Debug, Deserialize, Serialize)]
pub struct KbFile {
    pub schema_version: u32,
    /// `None` = 文件没有声明；运行期 overlay 据此决定是否覆盖默认值。
    #[serde(default)]
    pub default_effect: Option<Effect>,
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
    /// 显式只读分类：只有确定不会写盘、不会执行子进程的命令才可标记。
    /// 默认 false（fail-closed）。
    #[serde(default)]
    pub read_only: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubcommandMode {
    /// name 是**同义组**：任一元素等于 `args[0]` 即命中（如 ["get","logs"]）。
    #[default]
    Any,
    /// name 是**命令前缀序列**：`args` 必须以 name 开头（如 ["approval","list"]）。
    /// 用于避免 `approval approve` 被 `["approval","list"]` 的同义组语义误放行。
    Sequence,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SubcommandEntry {
    /// 子命令 token 序列，如 ["get"] / ["rollout", "restart"]
    pub name: Vec<String>,
    /// name 的匹配语义。默认 `any`（同义组），保持既有数据行为。
    #[serde(default)]
    pub mode: SubcommandMode,
    pub effect: Effect,
    #[serde(default)]
    pub dangerous_flags: Vec<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// 显式只读分类，语义同 `CommandEntry::read_only`。
    #[serde(default)]
    pub read_only: Option<bool>,
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
            default_effect: file.default_effect.unwrap_or_default(),
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
        // 运行期 overlay 可以改默认判定（permissive 模式依赖它把未命中命令放行）。
        // 只有显式声明才覆盖，避免把"未声明"误当成 Ask。
        if let Some(effect) = file.default_effect {
            self.default_effect = effect;
        }
        for entry in file.command {
            self.commands.insert(entry.name.clone(), entry);
        }
        Ok(())
    }

    pub fn default_effect(&self) -> Effect {
        self.default_effect
    }

    /// 已加载的命令条目数（供 reload_config 回显与覆盖度守卫使用）。
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// 查询：返回 (Effect, source 描述)。`args` 为 program 之后的参数。
    pub fn lookup(&self, program: &str, args: &[String]) -> (Effect, String) {
        let (effect, source, _) = self.lookup_with_meta(program, args);
        (effect, source)
    }

    /// 查询并返回只读分类。`read_only` 只有在命令被显式标记、且最终动作是
    /// `Allow`、且未命中危险 flag 时才为 true。
    pub fn lookup_with_meta(&self, program: &str, args: &[String]) -> (Effect, String, bool) {
        let Some(entry) = self.commands.get(basename(program)) else {
            return (self.default_effect, "unknown".to_string(), false);
        };

        // 子命令匹配，取最长命中。
        // `name` 既可表示多 token 子命令序列（args 以其为前缀，如 ["rollout", "restart"]），
        // 也可表示一组同义子命令（任一元素等于 args[0]，如 ["get", "logs", "describe"]）。
        let mut best: Option<(&SubcommandEntry, usize)> = None;
        for sub in &entry.subcommand {
            let matched_len = match sub.mode {
                SubcommandMode::Sequence => {
                    if args.starts_with(sub.name.as_slice()) {
                        sub.name.len()
                    } else {
                        continue;
                    }
                }
                SubcommandMode::Any => {
                    if args.first().is_some_and(|a| sub.name.contains(a)) {
                        1
                    } else {
                        continue;
                    }
                }
            };
            if best.map(|(_, l)| matched_len > l).unwrap_or(true) {
                best = Some((sub, matched_len));
            }
        }

        let (mut effect, mut source, sub_flags, mut read_only) = match best {
            Some((sub, _)) => (
                sub.effect,
                format!("kb:{} {}", entry.name, sub.name.join(" ")),
                sub.dangerous_flags.as_slice(),
                sub.read_only.unwrap_or(false),
            ),
            None => (
                entry.effect.unwrap_or(self.default_effect),
                format!("kb:{}", entry.name),
                &[][..],
                entry.read_only.unwrap_or(false),
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
            read_only = false;
        }

        // Only an explicitly allowed command can be read-only. Ask/Deny are
        // never eligible, regardless of the entry's marker.
        if effect != Effect::Allow {
            read_only = false;
        }

        (effect, source, read_only)
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
        for cmd in ["ps", "top", "netstat", "ss", "du", "df", "sort"] {
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

    /// 威胁模型重定：以下命令能执行任意代码、逃逸到 shell、外联或破坏主机，
    /// **不得**被判为 allow。列表与 `scripts/kb-fuse-safe-chains.py` 的 FORCE_ASK
    /// 及 `data/commands.toml` 的人工条目保持一致。
    #[test]
    fn high_risk_commands_are_never_allowed() {
        let kb = KnowledgeBase::builtin().unwrap();
        const MUST_NOT_ALLOW: &[&str] = &[
            "python3",
            "perl",
            "ruby",
            "node",
            "nodejs",
            "php",
            "lua",
            "tclsh",
            "expect",
            "R",
            "Rscript",
            "awk",
            "gawk",
            "mawk",
            "nawk",
            "zsh",
            "vim",
            "nvim",
            "emacs",
            "nano",
            "less",
            "more",
            "man",
            "screen",
            "tmux",
            "watch",
            "nc",
            "ncat",
            "netcat",
            "socat",
            "ssh",
            "tcpdump",
            "sqlite3",
            "mysql",
            "psql",
            "redis-cli",
            "mongo",
            "mongosh",
            "pip",
            "pip3",
            "npm",
            "npx",
            "make",
            "ipython",
            "nu",
            "deno",
            "julia",
            "erl",
            "groovy",
            "scala",
            "sbcl",
            "swipl",
            "csi",
            "ts-node",
            "nodemon",
            "pry",
            "java",
            "mix",
            "phpunit",
            "luarocks",
            "dc",
            "mosh",
            "autossh",
            "kill",
            "pkill",
            "killall",
            "crontab",
            "sysctl",
            "mount",
        ];
        for cmd in MUST_NOT_ALLOW {
            assert!(
                kb.lookup(cmd, &[]).0 >= Effect::Ask,
                "`{cmd}` 属于任意代码执行/逃逸/外联/主机管理类，不得 allow"
            );
        }
    }

    /// 运维 CLI 的放行必须停留在子命令粒度：只读/文件传输放行，
    /// 审批动作与交互终端保持 ask（否则 agent 可自我批准或绕开闸门）。
    #[test]
    fn ops_cli_subcommands_are_scoped() {
        let kb = KnowledgeBase::builtin().unwrap();
        let args = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();

        assert_eq!(
            kb.lookup("clum-mcp", &args(&["approval", "list"])).0,
            Effect::Allow
        );
        assert_eq!(
            kb.lookup("clum-mcp", &args(&["audit", "verify"])).0,
            Effect::Allow
        );
        assert_eq!(
            kb.lookup("clum-mcp", &args(&["approval", "approve", "x"]))
                .0,
            Effect::Ask,
            "agent must never be able to self-approve"
        );
        assert_eq!(
            kb.lookup("clum-mcp", &args(&["approval", "deny", "x"])).0,
            Effect::Ask
        );

        assert_eq!(
            kb.lookup("clum-cli", &args(&["push", "host", "a", "b"])).0,
            Effect::Allow
        );
        assert_eq!(
            kb.lookup("clum-cli", &args(&["pull", "host", "a", "b"])).0,
            Effect::Allow
        );
        assert_eq!(
            kb.lookup("clum-cli", &args(&["term", "host"])).0,
            Effect::Ask,
            "interactive term would bypass the command gate"
        );
        assert_eq!(
            kb.lookup("clum-cli", &args(&["forward", "host"])).0,
            Effect::Ask,
            "forward opens a tunnel and must stay gated"
        );
    }

    /// 文档中的知识库条数必须与 `builtin()` 的实际条数一致。
    /// 历史漂移：README 写 1,663（三层加和未去重），实际是 1,594。
    #[test]
    fn readonly_whitelist_never_contains_write_verbs() {
        let kb = KnowledgeBase::builtin().unwrap();
        let args = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();

        for cmd in ["ls", "cat", "grep", "ps", "stat", "df", "jq"] {
            assert!(
                kb.lookup_with_meta(cmd, &[]).2,
                "`{cmd}` should be read-only"
            );
        }
        assert!(
            kb.lookup_with_meta("systemctl", &args(&["status", "nginx"]))
                .2
        );
        assert!(kb.lookup_with_meta("kubectl", &args(&["get", "pods"])).2);
        assert!(kb.lookup_with_meta("git", &args(&["status"])).2);

        for cmd in [
            "rm", "mv", "dd", "tee", "sed", "awk", "find", "xargs", "sudo", "bash",
        ] {
            assert!(
                !kb.lookup_with_meta(cmd, &[]).2,
                "`{cmd}` must not be read-only"
            );
        }
        assert!(
            !kb.lookup_with_meta("systemctl", &args(&["restart", "nginx"]))
                .2
        );
        assert!(
            !kb.lookup_with_meta("kubectl", &args(&["delete", "pod", "x"]))
                .2
        );
        assert!(!kb.lookup_with_meta("git", &args(&["push"])).2);
        assert!(!kb.lookup_with_meta("git", &args(&["log", "--output=f"])).2);
        assert!(!kb.lookup_with_meta("rg", &args(&["--pre", "sh", "x"])).2);
        // `ip addr add ...` is a mutation: do not mark the `ip` family read-only.
        assert!(
            !kb.lookup_with_meta("ip", &args(&["addr", "add", "1.2.3.4/32", "dev", "eth0"]))
                .2
        );
        assert!(!kb.lookup_with_meta("date", &args(&["-s", "2026-01-01"])).2);
        assert!(
            !kb.lookup_with_meta("journalctl", &args(&["--vacuum-size=500M"]))
                .2,
            "vacuum writes and must not be read-only"
        );
    }

    #[test]
    fn documented_entry_count_matches_builtin() {
        fn with_thousands(n: usize) -> String {
            let s = n.to_string();
            let mut out = String::new();
            for (i, c) in s.chars().enumerate() {
                if i > 0 && (s.len() - i).is_multiple_of(3) {
                    out.push(',');
                }
                out.push(c);
            }
            out
        }

        let n = KnowledgeBase::builtin().unwrap().len();
        let formatted = with_thousands(n);
        let readme = include_str!("../../../README.md");
        let readme_zh = include_str!("../../../README.zh.md");
        assert!(
            readme.contains(&format!("{formatted}-entry knowledge base")),
            "README.md 的条数应与 builtin()={n} 一致（{formatted}）"
        );
        assert!(
            readme_zh.contains(&format!("{formatted} 条知识库")),
            "README.zh.md 的条数应与 builtin()={n} 一致（{formatted}）"
        );
    }
}
