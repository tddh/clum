use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

use super::ToolContext;
use clum_command_kb::EvalContext;

/// 携带可检查命令字符串的工具 → 从 args 中取出命令文本。
/// 返回 None 表示该工具没有可静态检查的命令（如 paste_buffer 的内容在远端缓冲区）。
///
/// **覆盖集是安全边界**：新增任何带 `command`/`keys`/`args` 语义的工具时，
/// 必须同步这里；`tests` 中有一条由 schema 驱动的守卫测试会自动发现遗漏。
pub(crate) const COMMAND_PAYLOAD_TOOLS: &[&str] = &[
    "exec",
    "shell_command",
    "batch_exec",
    "send_keys",
    "batch_send_keys",
    "broadcast_keys",
    "send_text",
    "respawn_pane",
    "split_pane_with",
    "cmd_escape",
];

/// 含同名字段但**不是命令语义**的工具（schema 守卫测试的白名单）。
pub(crate) const NON_COMMAND_PAYLOAD_TOOLS: &[&str] = &["wait_for_text"];

pub(crate) fn extract_command(tool_name: &str, args: &Value) -> Option<String> {
    match tool_name {
        "exec" | "shell_command" | "batch_exec" => args
            .get("command")
            .and_then(|v| v.as_str())
            .map(String::from),
        "send_keys" | "batch_send_keys" | "broadcast_keys" => {
            args.get("keys").and_then(|v| v.as_str()).map(String::from)
        }
        "send_text" => args.get("text").and_then(|v| v.as_str()).map(String::from),
        // respawn_pane / split_pane_with 的 `command` + `args` 是真实执行载荷。
        // 缺少 command 时不能返回 None：respawn_pane 会重跑 pane 的既有进程规格，
        // 而该规格可能来自一次**已批准**的 `split_pane_with(command=...)`——不裁决
        // 意味着一次性审批可被无限重放。split_pane_with 的 command 是必填，缺失
        // 属畸形调用。两者都返回 KB 不可能命中的哨兵 → fail-closed 到 ask；
        // 管理员可用 tool 维度规则显式放行。
        "respawn_pane" => Some(
            command_with_extra_args(args)
                .unwrap_or_else(|| "respawn-pane:existing-process".to_string()),
        ),
        "split_pane_with" => Some(
            command_with_extra_args(args)
                .unwrap_or_else(|| "split-pane:missing-command".to_string()),
        ),
        // cmd_escape 的参数是 rmux CLI（tmux 兼容命令面），同样可能触达 shell。
        // 作为命令文本评估：未收录的 rmux 子命令会 fail-closed 到 ask。
        "cmd_escape" => args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty()),
        _ => None,
    }
}

fn command_with_extra_args(args: &Value) -> Option<String> {
    let command = args.get("command").and_then(|v| v.as_str())?;
    let extra: Vec<&str> = args
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if extra.is_empty() {
        Some(command.to_string())
    } else {
        Some(format!("{command} {}", extra.join(" ")))
    }
}

pub(crate) enum Verdict {
    Allow,
    Deny { source: String, reason: String },
    Ask { source: String, reason: String },
}

/// 一次工具调用的裁决结果（多主机场景已按最严聚合）。
pub(crate) struct CallEvaluation {
    pub verdict: Verdict,
    /// 审批绑定指纹（`sha256:…`）。审批表只存指纹，命令原文不落盘。
    pub fingerprint: String,
    /// 已脱敏的可读摘要（审计 detail 与审批列表展示用）
    pub display: String,
    /// 原始命令文本（仅命令类工具有）。用于回传给 agent 的 payload——
    /// 那是 agent 自己发出的内容，不构成泄露。
    pub command: Option<String>,
    /// 裁决详情（无命令载荷时为 None）
    pub decision: Option<clum_command_kb::Decision>,
    /// 命令被显式分类为只读（仅 `command:read` 模式使用）。
    pub read_only: bool,
}

impl Verdict {
    /// 严重度：Allow < Ask < Deny。多主机场景跨 group 取最严时使用。
    pub(crate) fn severity(&self) -> u8 {
        match self {
            Verdict::Allow => 0,
            Verdict::Ask { .. } => 1,
            Verdict::Deny { .. } => 2,
        }
    }
}

/// 对一次工具调用做裁决。
///
/// `host_groups` 是本次调用涉及的**所有**主机组（`batch_exec` 等 batch 类工具的目标主机
/// 在 `hosts` 数组里；只读 `args["host"]` 会让 host_group 恒为 None，使所有按主机组限定的
/// 规则被跳过）。逐个 group 评估后**取最严**，因此"任一主机命中严规则"即整体从严。
///
/// 返回 `None` 表示不裁决：该工具既无可静态检查的命令载荷、也没有命中任何 tool/group
/// 维度规则（保持「不检查」语义，不误升 ask）。
pub(crate) fn evaluate_call(
    ctx: &ToolContext,
    tool_name: &str,
    args: &Value,
    host_groups: &[Option<String>],
) -> Option<CallEvaluation> {
    let caller_group = ctx
        .caller_group
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let rules = ctx.rules.read().unwrap_or_else(|e| e.into_inner());
    let command_kb = ctx.command_kb.read().unwrap_or_else(|e| e.into_inner());

    let command = extract_command(tool_name, args);
    // 运行时守卫的 debug 版本：未登记却带载荷的工具，在 debug 构建下立即暴露。
    // 正式覆盖校验由 tests::every_payload_tool_is_adjudicated_or_whitelisted 完成。
    debug_assert!(
        COMMAND_PAYLOAD_TOOLS.contains(&tool_name)
            || NON_COMMAND_PAYLOAD_TOOLS.contains(&tool_name)
            || command.is_none(),
        "tool '{tool_name}' carries a command payload but is not registered in extract_command"
    );
    let fingerprint = call_fingerprint(tool_name, command.as_deref(), args);
    let display = command
        .as_deref()
        .map(redact_command_for_display)
        .unwrap_or_else(|| format!("<{tool_name}: 无可静态检查的命令载荷>"));

    // 空切片按「无主机组上下文」处理，保持既有语义
    let groups: &[Option<String>] = if host_groups.is_empty() {
        &[None]
    } else {
        host_groups
    };

    let mut best: Option<CallEvaluation> = None;
    for hg in groups {
        let eval_ctx = EvalContext {
            tool: tool_name.to_string(),
            caller_group: caller_group.clone(),
            host_group: hg.clone(),
        };
        let decision = match command.as_deref() {
            Some(cmd) => clum_command_kb::evaluate(&command_kb, &rules, cmd, &eval_ctx)
                .unwrap_or_else(|never| match never {}),
            // 无命令载荷（paste_buffer）：知识库无从分类，
            // 只按 tool/group 维度匹配规则——这让 command-rules.toml 里「堵住不可静态
            // 检查的旁路通道」的示例规则真正生效。未命中则不裁决（continue）。
            None => {
                match rules.match_rule(tool_name, "", &[], caller_group.as_deref(), hg.as_deref()) {
                    Some(rule) => rule_decision(rule),
                    None => continue,
                }
            }
        };
        let candidate = CallEvaluation {
            verdict: verdict_of(&decision),
            fingerprint: fingerprint.clone(),
            display: display.clone(),
            command: command.clone(),
            read_only: decision.read_only,
            decision: Some(decision),
        };
        let stricter = match &best {
            None => true,
            Some(prev) => candidate.verdict.severity() > prev.verdict.severity(),
        };
        if stricter {
            best = Some(candidate);
        }
    }
    best
}

/// 由命中的规则直接构造 Decision（无命令载荷路径用；不经知识库分类）。
fn rule_decision(rule: &clum_command_kb::rules::Rule) -> clum_command_kb::Decision {
    clum_command_kb::Decision {
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
}

/// 三态 → Verdict 映射（deny/ask 携带来源与理由，供错误信封与审计使用）。
fn verdict_of(decision: &clum_command_kb::Decision) -> Verdict {
    match decision.effect {
        clum_command_kb::Effect::Allow => Verdict::Allow,
        clum_command_kb::Effect::Deny => Verdict::Deny {
            source: decision.source.clone(),
            reason: decision.reason.clone(),
        },
        clum_command_kb::Effect::Ask => Verdict::Ask {
            source: decision.source.clone(),
            reason: decision.reason.clone(),
        },
    }
}

/// deny 的返回 payload（结构化的错误信封，enrich_error 会保留既有 error_code）
pub(crate) fn deny_payload(source: &str, reason: &str, command: &str) -> Value {
    json!({
        "ok": false,
        "error": format!("command denied by rule '{source}'"),
        "error_code": "COMMAND_DENIED",
        "recovery_hint": reason,
        "retryable": false,
        "rule_id": source,
        "command": command,
    })
}

/// 计算一次工具调用的**审批绑定指纹**。
///
/// 统一返回 `sha256:<hex>`：命令类工具对**命令原文**取哈希，无命令载荷的工具
/// （`paste_buffer` / `cmd_escape` / `respawn_pane`）对 `tool + 规范化 args` 取哈希。
/// 这样审批表里存的是指纹而非原文——命令中的密钥不会明文落盘。
/// 「原文精确匹配」语义在哈希下等价（SHA-256 无实际碰撞风险）。
/// 会改变实际执行体的参数。命令相同但这些字段不同 → 不是同一个执行请求，
/// 不能复用同一份批准（否则「批准 A、执行 A+LD_PRELOAD」）。
const EXEC_CONTEXT_FIELDS: &[&str] = &["env", "cwd", "shell", "kill"];

pub(crate) fn call_fingerprint(tool_name: &str, command: Option<&str>, args: &Value) -> String {
    let mut h = Sha256::new();
    h.update(tool_name.as_bytes());
    // NUL 分隔：避免 ("ab", Some("c")) 与 ("a", Some("bc")) 撞出同一指纹
    h.update([0u8]);
    match command {
        Some(c) => {
            h.update(c.as_bytes());
            for field in EXEC_CONTEXT_FIELDS {
                if let Some(v) = args.get(*field) {
                    h.update([0u8]);
                    h.update(field.as_bytes());
                    h.update([b'=']);
                    h.update(serde_json::to_string(v).unwrap_or_default().as_bytes());
                }
            }
        }
        // 无命令载荷：用规范化 args。serde_json 未开 preserve_order → Map 是 BTreeMap，
        // 键序稳定，故等价参数得到同一指纹（有测试固化该假设）。
        // `approval_id` 是重试凭证，不能参与指纹：首次 ask 时 args 不含它，
        // 批准后带 id 重发必须得到同一指纹，否则 consume 永远 FingerprintMismatch。
        None => {
            let mut args = args.clone();
            if let Some(object) = args.as_object_mut() {
                object.remove("approval_id");
            }
            h.update(serde_json::to_string(&args).unwrap_or_default().as_bytes());
        }
    }
    format!("sha256:{}", hex::encode(h.finalize()))
}

/// 脱敏模式表：`(正则, 替换串)`。捕获组 1 保留前缀，只抹掉凭据本体。
fn redact_patterns() -> &'static [(Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // Authorization: Bearer <token> / 裸 bearer <token>
            (
                Regex::new(r"(?i)\b(bearer\s+)\S+").expect("static regex"),
                "${1}[REDACTED]",
            ),
            // --password=x / --password x / --token / --api-key / --secret / --auth
            // 注意**不能**加 `\b`：`空格 → -` 之间不存在词边界，加了会整条失配。
            (
                Regex::new(
                    r"(?i)(--?(?:password|passwd|pwd|token|api[-_]?key|access[-_]?key|secret|auth)[= ])\S+",
                )
                .expect("static regex"),
                "${1}[REDACTED]",
            ),
            // 环境变量赋值：API_TOKEN=x / DB_PASSWORD=x / SECRET_KEY=x
            (
                Regex::new(
                    r"(?i)\b([A-Za-z_][A-Za-z0-9_]*(?:password|passwd|token|secret|api_?key|access_?key)[A-Za-z0-9_]*=)\S+",
                )
                .expect("static regex"),
                "${1}[REDACTED]",
            ),
            // URL 内嵌凭据：scheme://user:pass@host（git clone / curl 常见）
            (
                Regex::new(r"://([^/\s:@]+):([^@\s]+)@").expect("static regex"),
                "://${1}:[REDACTED]@",
            ),
        ]
    })
}

/// 展示用脱敏：把命令中的凭据替换为 `[REDACTED]`。
///
/// 只处理**高置信度**模式（`Bearer x`、`--password=x`、`*TOKEN*=x`、URL 内嵌凭据），
/// 不做熵检测——误判会让审批列表变得不可读，人类反而更容易橡皮图章。
/// 仅用于**展示与审计**；审批绑定用的是 `call_fingerprint` 的哈希，不受脱敏影响。
pub(crate) fn redact_command_for_display(cmd: &str) -> String {
    let mut out = cmd.to_string();
    for (re, replacement) in redact_patterns() {
        out = re.replace_all(&out, *replacement).to_string();
    }
    out
}

/// ask 的返回 payload（含 approval_id，人工凭此放行）
pub(crate) fn ask_payload(
    approval_id: &str,
    source: &str,
    reason: &str,
    command: &str,
    expires_at: &str,
) -> Value {
    json!({
        "ok": false,
        "error": format!("command requires approval (rule '{source}')"),
        "error_code": "APPROVAL_REQUIRED",
        "recovery_hint": format!(
            "approval id {approval_id} expires at {expires_at}; a human with server access must run: clum-mcp approval approve {approval_id}"
        ),
        "retryable": false,
        "approval_id": approval_id,
        "rule_id": source,
        "reason": reason,
        "expires_at": expires_at,
        "command": command,
    })
}

/// approval_id 校验失败的返回 payload。
///
/// 一律拒绝、`retryable: false`：放行校验的任何一环不通过都不得降级为执行。
/// 审批是**一次性**授权，被消费或校验失败后必须重新申请。
pub(crate) fn approval_invalid_payload(reason: &str, approval_id: &str) -> Value {
    json!({
        "ok": false,
        "error": format!("approval '{approval_id}' cannot be used: {reason}"),
        "error_code": "APPROVAL_INVALID",
        "recovery_hint": "the approval was NOT consumed by this attempt unless stated otherwise; \
                         re-run without approval_id to request a fresh one. The command, tool and \
                         host must be byte-identical to the approved request — approvals are \
                         one-shot and bound to a single command on a single host.",
        "retryable": false,
        "approval_id": approval_id,
        "reason": reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_command_per_tool() {
        assert_eq!(
            extract_command("exec", &json!({"command": "ls -la"})).as_deref(),
            Some("ls -la")
        );
        assert_eq!(
            extract_command("shell_command", &json!({"command": "df -h"})).as_deref(),
            Some("df -h")
        );
        assert_eq!(
            extract_command("batch_exec", &json!({"command": "df -h"})).as_deref(),
            Some("df -h")
        );
        assert_eq!(
            extract_command("send_keys", &json!({"keys": "rm -rf /\n"})).as_deref(),
            Some("rm -rf /\n")
        );
        assert_eq!(
            extract_command("batch_send_keys", &json!({"keys": "ls\n"})).as_deref(),
            Some("ls\n")
        );
        assert_eq!(
            extract_command("send_text", &json!({"text": "hi"})).as_deref(),
            Some("hi")
        );
        assert_eq!(
            extract_command("broadcast_keys", &json!({"keys": "rm -rf /\n"})).as_deref(),
            Some("rm -rf /\n")
        );
        assert_eq!(extract_command("capture_pane", &json!({})), None);
        assert_eq!(extract_command("paste_buffer", &json!({})), None);

        // 曾经被漏掉的执行载荷通道
        assert_eq!(
            extract_command(
                "respawn_pane",
                &json!({"pane_id": "%0", "command": "rm", "args": ["-rf", "/"]})
            )
            .as_deref(),
            Some("rm -rf /")
        );
        assert_eq!(
            extract_command("respawn_pane", &json!({"pane_id": "%0"})).as_deref(),
            Some("respawn-pane:existing-process"),
            "bare respawn must fail closed (it can replay an approved process spec)"
        );
        assert_eq!(
            extract_command("split_pane_with", &json!({"command": "rm -rf /"})).as_deref(),
            Some("rm -rf /")
        );
        assert_eq!(
            extract_command("cmd_escape", &json!({"args": ["kill-session", "-t", "x"]})).as_deref(),
            Some("kill-session -t x")
        );
    }

    /// 覆盖守卫：schema 中所有带 `command`/`keys`/`text`/`args` 的工具，必须被
    /// `extract_command` 覆盖，或显式登记为非命令语义。新增工具忘记登记时这条测试失败。
    ///
    /// 历史教训：`broadcast_keys`、`split_pane_with`、`respawn_pane` 均曾因人工清单
    /// 遗漏而绕过闸门——同类失误不能靠记忆防住。
    #[test]
    fn every_payload_tool_is_adjudicated_or_whitelisted() {
        let def = crate::schema::tools_definition();
        let tools = def["tools"].as_array().expect("tools array");
        let mut missing = Vec::new();
        for tool in tools {
            let name = tool["name"].as_str().unwrap_or_default();
            let Some(props) = tool["inputSchema"]["properties"].as_object() else {
                continue;
            };
            let has_payload = ["command", "keys", "text", "args"]
                .iter()
                .any(|k| props.contains_key(*k));
            if !has_payload {
                continue;
            }
            if COMMAND_PAYLOAD_TOOLS.contains(&name) || NON_COMMAND_PAYLOAD_TOOLS.contains(&name) {
                continue;
            }
            missing.push(name.to_string());
        }
        assert!(
            missing.is_empty(),
            "这些工具带命令载荷但未登记到 extract_command: {missing:?}"
        );
    }

    #[test]
    fn fingerprint_binds_execution_context() {
        let base_args = json!({"host": "tf01", "pane_id": "%0", "command": "nginx"});
        let base = call_fingerprint("respawn_pane", Some("nginx"), &base_args);

        for (field, value) in [
            ("env", json!({"LD_PRELOAD": "/tmp/evil.so"})),
            ("cwd", json!("/tmp")),
            ("shell", json!(false)),
            ("kill", json!(true)),
        ] {
            let mut args = base_args.clone();
            args[field] = value;
            assert_ne!(
                base,
                call_fingerprint("respawn_pane", Some("nginx"), &args),
                "{field} changes the executed process and must change the fingerprint"
            );
        }

        // approval_id 是重试凭证，不能影响指纹（否则批准后重发必失配）。
        let mut with_id = base_args.clone();
        with_id["approval_id"] = json!("ap-1");
        assert_eq!(
            base,
            call_fingerprint("respawn_pane", Some("nginx"), &with_id)
        );
    }

    #[test]
    fn deny_payload_carries_denied_code_and_rule() {
        let p = deny_payload("rule:no-rm", "destructive", "rm -rf /");
        assert_eq!(p["ok"], false);
        assert_eq!(p["error_code"], "COMMAND_DENIED");
        assert_eq!(p["rule_id"], "rule:no-rm");
        assert_eq!(p["recovery_hint"], "destructive");
        assert!(p["error"].as_str().unwrap().contains("denied by rule"));
    }

    #[test]
    fn ask_payload_carries_approval_metadata() {
        let p = ask_payload(
            "ap-1",
            "rule:prod",
            "prod change",
            "systemctl stop nginx",
            "eod",
        );
        assert_eq!(p["ok"], false);
        assert_eq!(p["error_code"], "APPROVAL_REQUIRED");
        assert_eq!(p["approval_id"], "ap-1");
        assert_eq!(p["rule_id"], "rule:prod");
        assert_eq!(p["expires_at"], "eod");
        assert!(p["recovery_hint"]
            .as_str()
            .unwrap()
            .contains("clum-mcp approval approve ap-1"));
    }

    // ── call_fingerprint ────────────────────────────────────────

    #[test]
    fn fingerprint_is_stable_and_hides_plaintext() {
        let args = json!({"host": "tf01", "command": "systemctl restart nginx"});
        let f1 = call_fingerprint("exec", Some("systemctl restart nginx"), &args);
        let f2 = call_fingerprint("exec", Some("systemctl restart nginx"), &args);

        assert_eq!(f1, f2, "同输入必须得到同指纹（否则批准无法匹配）");
        assert!(
            f1.starts_with("sha256:"),
            "格式应为 sha256:<hex>，实际 {f1}"
        );
        assert_eq!(f1.len(), "sha256:".len() + 64, "应为 64 位十六进制");
        assert!(
            !f1.contains("systemctl"),
            "命令原文不得出现在指纹里（审批表只存指纹，避免明文落盘）"
        );
    }

    #[test]
    fn fingerprint_binds_tool_and_command() {
        let args = json!({"host": "tf01", "command": "systemctl restart nginx"});
        let base = call_fingerprint("exec", Some("systemctl restart nginx"), &args);

        assert_ne!(
            base,
            call_fingerprint("send_keys", Some("systemctl restart nginx"), &args),
            "换工具必须换指纹：批准不得跨工具复用"
        );
        assert_ne!(
            base,
            call_fingerprint("exec", Some("systemctl stop nginx"), &args),
            "改命令必须换指纹：防「批 A 执行 B」偷渡"
        );
    }

    #[test]
    fn fingerprint_for_commandless_tool_uses_args_and_ignores_key_order() {
        let a1 = json!({"host": "tf01", "buffer_name": "b0"});
        let a2 = json!({"host": "tf01", "buffer_name": "b1"});
        let f1 = call_fingerprint("paste_buffer", None, &a1);

        assert!(f1.starts_with("sha256:"));
        assert_ne!(
            f1,
            call_fingerprint("paste_buffer", None, &a2),
            "不同参数必须得到不同指纹"
        );

        // serde_json 未开 preserve_order → Map 是 BTreeMap，键序稳定。
        // 若将来开启 preserve_order，此断言会失败并提醒此处需要显式排序。
        let reordered = json!({"buffer_name": "b0", "host": "tf01"});
        assert_eq!(
            f1,
            call_fingerprint("paste_buffer", None, &reordered),
            "键序不同的等价参数应得到同指纹"
        );
    }

    #[test]
    fn fingerprint_for_commandless_tool_ignores_approval_id() {
        let base = json!({"host": "tf01", "buffer_name": "b0"});
        let with_id = json!({"host": "tf01", "buffer_name": "b0", "approval_id": "ap-1"});
        assert_eq!(
            call_fingerprint("paste_buffer", None, &base),
            call_fingerprint("paste_buffer", None, &with_id),
            "approval_id is a retry credential; including it breaks one-shot consumption"
        );
    }

    // ── redact_command_for_display ──────────────────────────────

    #[test]
    fn redact_hides_credentials() {
        for (cmd, secret) in [
            (
                "curl -H \"Authorization: Bearer sk-abc123\" https://x",
                "sk-abc123",
            ),
            ("mysql --password=hunter2 -u root", "hunter2"),
            ("mysql --password hunter2 -u root", "hunter2"),
            ("API_TOKEN=xyz123 some-cmd", "xyz123"),
            ("deploy --api-key ABCDEF123456", "ABCDEF123456"),
            (
                "git clone https://user:ghp_secret9@github.com/x",
                "ghp_secret9",
            ),
        ] {
            let out = redact_command_for_display(cmd);
            assert!(!out.contains(secret), "凭据必须被脱敏：{cmd:?} → {out:?}");
            assert!(out.contains("[REDACTED]"), "应留下脱敏标记：{out:?}");
        }
    }

    #[test]
    fn redact_preserves_normal_commands() {
        // 误伤普通命令会让审批列表不可读，人类反而更容易橡皮图章
        for cmd in [
            "ls -la /tmp",
            "kubectl get pods -n prod",
            "grep error /var/log/app.log",
            "systemctl restart nginx",
            "tar -czf backup.tgz /data",
        ] {
            assert_eq!(
                redact_command_for_display(cmd),
                cmd,
                "普通命令不得被改动：{cmd:?}"
            );
        }
    }
}
