//! 结构化错误分类：把 anyhow 错误链和 bridge 返回的错误字符串映射为稳定的
//! error_code + recovery_hint + retryable，供 tools/call 统一信封使用。
//!
//! 设计原则：只增不改。`error` 保留原始字符串（向后兼容），新增三个字段，
//! Agent 可凭 error_code 可靠分支，凭 recovery_hint 决定下一步动作。
//! 分类基于消息子串匹配（bridge 暂不带错误码，后续桥侧升级后此处优先采用桥侧码）。

use serde_json::{json, Value};

pub struct Classified {
    pub code: &'static str,
    pub hint: &'static str,
    pub retryable: bool,
}

const fn c(code: &'static str, hint: &'static str, retryable: bool) -> Classified {
    Classified {
        code,
        hint,
        retryable,
    }
}

/// 按错误消息分类。匹配顺序即优先级：更具体的模式在前。
pub fn classify_message(msg: &str) -> Classified {
    let m = msg.to_lowercase();
    let has = |p: &str| m.contains(p);

    if has("not in your group") || has("forbidden") {
        return c(
            "FORBIDDEN",
            "该主机不在你的分组内，联系管理员确认分组分配",
            false,
        );
    }
    if has("host not found") {
        return c("HOST_NOT_FOUND", "host_list 检查可用主机名", false);
    }
    if has("missing '") {
        return c(
            "INVALID_PARAMS",
            "缺少必填参数，对照 tools/list 中该工具的 inputSchema.required",
            false,
        );
    }
    if has("pane id") && has("not found") || has("can't find pane") || has("pane not found") {
        return c(
            "PANE_NOT_FOUND",
            "list_window_panes 确认当前 pane_id（pane 可能已关闭）",
            false,
        );
    }
    if has("session already exists") || has("duplicate session") {
        return c(
            "SESSION_EXISTS",
            "会话已存在，直接 session_attach 或换个名称",
            false,
        );
    }
    if has("session not found") || has("can't find session") || has("no such session") {
        return c("SESSION_NOT_FOUND", "session_create 创建会话", false);
    }
    if has("window not found") || has("can't find window") {
        return c(
            "WINDOW_NOT_FOUND",
            "window_info / select_window 确认窗口存在",
            false,
        );
    }
    if has("forward not found") {
        return c("FORWARD_NOT_FOUND", "forward_list 确认转发 ID", false);
    }
    if has("pane still active") {
        return c(
            "PANE_BUSY",
            "pane 非空闲：先 close_pane 或换 pane，或 respawn_pane(kill=true)",
            false,
        );
    }
    if has("path traversal") || has("unsafe relative path") {
        return c(
            "PATH_TRAVERSAL",
            "路径不能包含 '..' 且下载需相对路径：修正路径后重试",
            false,
        );
    }
    if has("not in allowed list") {
        return c(
            "FORWARD_DENIED",
            "转发目标不在白名单：检查 hosts.yaml 的 allowed_forward_targets",
            false,
        );
    }
    if has("authentication failed") || has("auth failed") {
        return c(
            "AUTH_FAILED",
            "检查 hosts.yaml 的 bridge_token 与 bridge 一致",
            false,
        );
    }
    if has("connection refused") {
        return c(
            "BRIDGE_UNREACHABLE",
            "bridge 未运行：systemctl status rmux-bridge 确认后重试",
            true,
        );
    }
    if has("connection lost") || has("connection reset") {
        return c(
            "CONNECTION_LOST",
            "bridge 重启或网络中断，等待几秒后重试",
            true,
        );
    }
    if has("timeout") || has("timed out") {
        return c(
            "TIMEOUT",
            "exec 超时不杀进程：capture_pane 查看进度、wait_for_text 等完成，不要盲目重跑；若是连接超时则确认主机在线、Server 9788 端口可达",
            false,
        );
    }
    c(
        "UNKNOWN",
        "查看 error 详情；必要时 capture_pane 检查终端状态后重试",
        false,
    )
}

/// 业务失败增强：工具返回 ok:false 时补齐结构化字段（幂等）。
/// refused（exec 安全检查）单独处理：error 字符串本身已是操作建议。
pub fn enrich_error(result: &mut Value) {
    let Some(obj) = result.as_object_mut() else {
        return;
    };
    if obj.get("ok").and_then(Value::as_bool) != Some(false) {
        return;
    }
    if obj.contains_key("error_code") {
        return;
    }
    let msg = obj
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if obj.get("refused").and_then(Value::as_bool) == Some(true) {
        obj.insert("error_code".into(), json!("REFUSED_STATE"));
        if !msg.is_empty() {
            obj.insert("recovery_hint".into(), json!(msg));
        }
        obj.insert("retryable".into(), json!(false));
        return;
    }

    let classified = classify_message(&msg);
    obj.insert("error_code".into(), json!(classified.code));
    obj.insert("recovery_hint".into(), json!(classified.hint));
    obj.insert("retryable".into(), json!(classified.retryable));
}

/// anyhow 错误 → 结构化失败 result（handler 的 Err 分支）。
pub fn error_result(e: &anyhow::Error) -> Value {
    let msg = format!("{e:#}");
    let classified = classify_message(&msg);
    json!({
        "ok": false,
        "error": msg,
        "error_code": classified.code,
        "recovery_hint": classified.hint,
        "retryable": classified.retryable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_registry_and_params() {
        let r = classify_message("host not found: tf99");
        assert_eq!(r.code, "HOST_NOT_FOUND");
        assert!(!r.retryable);

        let r = classify_message("missing 'pane_id'");
        assert_eq!(r.code, "INVALID_PARAMS");
    }

    #[test]
    fn classifies_terminal_objects() {
        let r = classify_message("pane id %99 was not found");
        assert_eq!(r.code, "PANE_NOT_FOUND");

        let r = classify_message("session not found");
        assert_eq!(r.code, "SESSION_NOT_FOUND");
        assert!(r.hint.contains("session_create"));

        let r = classify_message("session already exists: clum");
        assert_eq!(r.code, "SESSION_EXISTS");

        let r = classify_message("forward not found: abc");
        assert_eq!(r.code, "FORWARD_NOT_FOUND");

        let r = classify_message("pane still active");
        assert_eq!(r.code, "PANE_BUSY");
    }

    #[test]
    fn classifies_security_denials() {
        let r = classify_message("path traversal rejected: ../../etc");
        assert_eq!(r.code, "PATH_TRAVERSAL");

        let r = classify_message("forward target 10.0.0.1:22 not in allowed list for host 'tf001'");
        assert_eq!(r.code, "FORWARD_DENIED");

        let r = classify_message("bridge QUIC authentication failed");
        assert_eq!(r.code, "AUTH_FAILED");
    }

    #[test]
    fn classifies_network_conditions() {
        let r = classify_message("connection refused");
        assert_eq!(r.code, "BRIDGE_UNREACHABLE");
        assert!(r.retryable);

        let r = classify_message("recv: connection lost");
        assert_eq!(r.code, "CONNECTION_LOST");
        assert!(r.retryable);

        let r = classify_message("timeout waiting for sentinel after 15000ms");
        assert_eq!(r.code, "TIMEOUT");
        assert!(!r.retryable);
        assert!(r.hint.contains("不要盲目重跑"));
    }

    #[test]
    fn fallback_is_unknown() {
        let r = classify_message("something unexpected");
        assert_eq!(r.code, "UNKNOWN");
        assert!(!r.retryable);
    }

    #[test]
    fn enrich_adds_fields_to_business_failure() {
        let mut v = json!({"ok": false, "error": "pane id %99 was not found"});
        enrich_error(&mut v);
        assert_eq!(v["error_code"], "PANE_NOT_FOUND");
        assert!(v["recovery_hint"]
            .as_str()
            .unwrap()
            .contains("list_window_panes"));
        assert_eq!(v["retryable"], false);
        assert_eq!(v["error"], "pane id %99 was not found"); // 原字符串保留
    }

    #[test]
    fn enrich_special_cases_refused() {
        let mut v = json!({
            "ok": false,
            "error": "A process is still running. Use wait_stable/wait_exit.",
            "refused": true
        });
        enrich_error(&mut v);
        assert_eq!(v["error_code"], "REFUSED_STATE");
        assert!(v["recovery_hint"].as_str().unwrap().contains("wait_stable"));
    }

    #[test]
    fn enrich_skips_success_and_is_idempotent() {
        let mut ok = json!({"ok": true, "output": "hi"});
        enrich_error(&mut ok);
        assert!(!ok.as_object().unwrap().contains_key("error_code"));

        let mut tagged = json!({"ok": false, "error": "x", "error_code": "CUSTOM"});
        enrich_error(&mut tagged);
        assert_eq!(tagged["error_code"], "CUSTOM");
        assert!(!tagged.as_object().unwrap().contains_key("recovery_hint"));
    }

    #[test]
    fn error_result_flattens_anyhow_chain() {
        let e = anyhow::anyhow!("host not found: tf99");
        let v = error_result(&e);
        assert_eq!(v["ok"], false);
        assert_eq!(v["error_code"], "HOST_NOT_FOUND");
        assert_eq!(v["error"], "host not found: tf99");
        assert_eq!(v["retryable"], false);
    }

    #[test]
    fn classify_rbac_forbidden() {
        let r = classify_message("host tf01 not in your group");
        assert_eq!(r.code, "FORBIDDEN");
        assert!(!r.retryable);
        assert!(r.hint.contains("分组"));

        let r = classify_message("forbidden: access denied");
        assert_eq!(r.code, "FORBIDDEN");
        assert!(!r.retryable);
    }

    #[test]
    fn classify_window_not_found() {
        let r = classify_message("window not found");
        assert_eq!(r.code, "WINDOW_NOT_FOUND");
        assert!(!r.retryable);
        assert!(r.hint.contains("window_info"));

        let r = classify_message("can't find window 3");
        assert_eq!(r.code, "WINDOW_NOT_FOUND");
    }

    #[test]
    fn classify_case_insensitive() {
        let r = classify_message("HOST NOT FOUND: tf99");
        assert_eq!(r.code, "HOST_NOT_FOUND");

        let r = classify_message("Connection REFUSED");
        assert_eq!(r.code, "BRIDGE_UNREACHABLE");
        assert!(r.retryable);

        let r = classify_message("FORBIDDEN");
        assert_eq!(r.code, "FORBIDDEN");
    }

    #[test]
    fn enrich_error_idempotent_on_non_object() {
        let mut v = json!(42);
        enrich_error(&mut v);
        assert_eq!(v, json!(42));
    }

    #[test]
    fn enrich_error_idempotent_on_ok_true() {
        let mut v = json!({"ok": true, "error": "host not found: tf99"});
        enrich_error(&mut v);
        assert!(!v.as_object().unwrap().contains_key("error_code"));
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn error_result_with_anyhow_context() {
        let e = anyhow::anyhow!("host not found: tf99")
            .context("middle wrapper")
            .context("outer context");
        let v = error_result(&e);
        assert_eq!(v["ok"], false);
        assert_eq!(v["error_code"], "HOST_NOT_FOUND");
        let error_str = v["error"].as_str().unwrap();
        assert!(error_str.contains("host not found"));
        assert!(error_str.contains("outer context"));
        assert_eq!(v["retryable"], false);
    }
}
