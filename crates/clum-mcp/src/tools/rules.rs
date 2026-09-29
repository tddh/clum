use serde_json::{json, Value};

use super::ToolContext;
use clum_command_kb::EvalContext;

/// 携带可检查命令字符串的工具 → 从 args 中取出命令文本。
/// 返回 None 表示该工具没有可静态检查的命令（如 paste_buffer 的内容在远端缓冲区）。
pub(crate) fn extract_command(tool_name: &str, args: &Value) -> Option<String> {
    match tool_name {
        "exec" | "shell_command" | "batch_exec" => args
            .get("command")
            .and_then(|v| v.as_str())
            .map(String::from),
        "send_keys" | "batch_send_keys" => {
            args.get("keys").and_then(|v| v.as_str()).map(String::from)
        }
        "send_text" => args.get("text").and_then(|v| v.as_str()).map(String::from),
        _ => None,
    }
}

pub(crate) enum Verdict {
    Allow,
    Deny { source: String, reason: String },
    Ask { source: String, reason: String },
}

/// 对一次工具调用做裁决。返回 `(裁决, 命令原文, 裁决详情)`；
/// `None` 表示该工具没有可静态检查的命令字符串（如 paste_buffer）。
pub(crate) fn evaluate_call(
    ctx: &ToolContext,
    tool_name: &str,
    args: &Value,
    host_group: Option<String>,
) -> Option<(Verdict, String, clum_command_kb::Decision)> {
    let command = extract_command(tool_name, args)?;
    let caller_group = ctx
        .caller_group
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let rules = ctx.rules.read().unwrap_or_else(|e| e.into_inner());

    let decision = clum_command_kb::evaluate(
        &ctx.command_kb,
        &rules,
        &command,
        &EvalContext {
            tool: tool_name.to_string(),
            caller_group,
            host_group,
        },
    )
    .unwrap_or_else(|never| match never {});

    let verdict = match decision.effect {
        clum_command_kb::Effect::Allow => Verdict::Allow,
        clum_command_kb::Effect::Deny => Verdict::Deny {
            source: decision.source.clone(),
            reason: decision.reason.clone(),
        },
        clum_command_kb::Effect::Ask => Verdict::Ask {
            source: decision.source.clone(),
            reason: decision.reason.clone(),
        },
    };

    Some((verdict, command, decision))
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
        assert_eq!(extract_command("capture_pane", &json!({})), None);
        assert_eq!(extract_command("paste_buffer", &json!({})), None);
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
}
