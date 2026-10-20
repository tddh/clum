//! 守护测试：仓库内随代码发布的策略文件必须能被真实解析。
//!
//! 这两个文件在 server 启动时加载；语法错误会导致启动失败。放在这里而不是
//! 单测里，是因为它们属于部署产物，不是库 API。

use std::path::PathBuf;

fn repo_config(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../config")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn shipped_command_rules_parse() {
    let text = repo_config("command-rules.toml");
    clum_command_kb::RuleSet::from_toml_str(&text).expect("config/command-rules.toml 必须可解析");
}

#[test]
fn shipped_command_kb_override_parses_and_sets_default_allow() {
    let text = repo_config("command-kb.toml");
    let mut kb = clum_command_kb::KnowledgeBase::builtin().expect("builtin kb");
    kb.merge_overlay_str(&text)
        .expect("config/command-kb.toml 必须可解析");
    assert_eq!(
        kb.default_effect(),
        clum_command_kb::Effect::Allow,
        "permissive 策略依赖运行期覆盖把未命中命令放行"
    );
}
