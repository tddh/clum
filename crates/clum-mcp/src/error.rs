//! 结构化错误分类：把 anyhow 错误链和 bridge 返回的错误字符串映射为稳定的
//! error_code + recovery_hint + retryable，供 tools/call 统一信封使用。
//!
//! 设计原则：只增不改。`error` 保留原始字符串（向后兼容），新增三个字段，
//! Agent 可凭 error_code 可靠分支，凭 recovery_hint 决定下一步动作。
//! 错误码分类逻辑下沉到 clum_core::error_code（与 bridge 出口注入共用），
//! 本模块只负责为每个 code 补充 recovery_hint / retryable。

use clum_core::error_code::*;
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

/// code → (recovery_hint, retryable)。code 由 clum_core::error_code::classify_error_message
/// 产生（与 bridge 侧注入一致），此处补充 UX 字段。
fn lookup(code: &str) -> Classified {
    match code {
        CODE_FORBIDDEN => c(
            "FORBIDDEN",
            "该主机不在你的分组内，联系管理员确认分组分配",
            false,
        ),
        CODE_HOST_NOT_FOUND => c("HOST_NOT_FOUND", "host_list 检查可用主机名", false),
        CODE_INVALID_PARAMS => c(
            "INVALID_PARAMS",
            "缺少或非法参数，对照 tools/list 中该工具的 inputSchema.required",
            false,
        ),
        CODE_PANE_NOT_FOUND => c(
            "PANE_NOT_FOUND",
            "list_window_panes 确认当前 pane_id（pane 可能已关闭）",
            false,
        ),
        CODE_SESSION_EXISTS => c(
            "SESSION_EXISTS",
            "会话已存在，直接 session_attach 或换个名称",
            false,
        ),
        CODE_SESSION_NOT_FOUND => c("SESSION_NOT_FOUND", "session_create 创建会话", false),
        CODE_WINDOW_NOT_FOUND => c(
            "WINDOW_NOT_FOUND",
            "window_info / select_window 确认窗口存在",
            false,
        ),
        CODE_FORWARD_NOT_FOUND => c("FORWARD_NOT_FOUND", "forward_list 确认转发 ID", false),
        CODE_PANE_BUSY => c(
            "PANE_BUSY",
            "pane 非空闲：先 close_pane 或换 pane，或 respawn_pane(kill=true)",
            false,
        ),
        CODE_PATH_TRAVERSAL => c(
            "PATH_TRAVERSAL",
            "路径不能包含 '..' 且下载需相对路径：修正路径后重试",
            false,
        ),
        CODE_FORWARD_DENIED => c(
            "FORWARD_DENIED",
            "转发目标不在白名单：检查 hosts.yaml 的 allowed_forward_targets",
            false,
        ),
        CODE_AUTH_FAILED => c(
            "AUTH_FAILED",
            "检查 hosts.yaml 的 bridge_token 与 bridge 一致（direct 模式）",
            false,
        ),
        CODE_BRIDGE_UNREACHABLE => c(
            "BRIDGE_UNREACHABLE",
            "bridge 未运行：systemctl status rmux-bridge 确认后重试",
            true,
        ),
        CODE_CONNECTION_LOST => c(
            "CONNECTION_LOST",
            "bridge 重启或网络中断，等待几秒后重试",
            true,
        ),
        CODE_CONNECT_TIMEOUT => c(
            "CONNECT_TIMEOUT",
            "连接超时：确认主机在线、Server 9788 端口可达后重试",
            true,
        ),
        CODE_TIMEOUT => c(
            "TIMEOUT",
            "exec 超时不杀进程：capture_pane 查看进度、wait_for_text 等完成，不要盲目重跑；若是连接超时则确认主机在线、Server 9788 端口可达",
            false,
        ),
        CODE_CLI_FAILED => c(
            "CLI_FAILED",
            "bridge 端 rmux CLI 回退失败：检查 rmux 安装完整性（rmux list-commands）",
            false,
        ),
        CODE_PROTOCOL_ERROR => c(
            "PROTOCOL_ERROR",
            "桥侧帧协议错误：检查 bridge 版本是否过旧，考虑升级",
            false,
        ),
        // 兜底：未知 code（如新 bridge 注入旧 MCP 不认识的码）。error_code 原值保留
        // （or_insert 不覆盖），retryable 按不可重试处理——版本偏差时可能丢失重试信号。
        _ => c(
            "UNKNOWN",
            "查看 error 详情；必要时 capture_pane 检查终端状态后重试",
            false,
        ),
    }
}

/// 按错误消息分类。匹配逻辑在 clum_core::error_code（与 bridge 出口注入共用），
/// 此处补充 recovery_hint / retryable。
pub fn classify_message(msg: &str) -> Classified {
    lookup(classify_error_message(msg))
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
    let code_owned = obj
        .get("error_code")
        .and_then(Value::as_str)
        .map(String::from);
    if let Some(code) = code_owned {
        // error_code 已存在（bridge 出口注入或工具内联）：补齐缺失的 UX 字段。
        // bridge 只注入 error_code，recovery_hint/retryable 由本层 lookup 提供，
        // 保证所有错误信封字段完整。
        if !obj.contains_key("recovery_hint") || !obj.contains_key("retryable") {
            let classified = lookup(&code);
            obj.entry("recovery_hint").or_insert(json!(classified.hint));
            obj.entry("retryable")
                .or_insert(json!(classified.retryable));
        }
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

        // 已带 error_code（bridge 注入或工具内联）时不覆盖 code，但补齐缺失的 UX 字段
        let mut tagged = json!({"ok": false, "error": "x", "error_code": "CUSTOM"});
        enrich_error(&mut tagged);
        assert_eq!(tagged["error_code"], "CUSTOM");
        assert!(tagged.as_object().unwrap().contains_key("recovery_hint"));
        assert_eq!(tagged["retryable"], false);
        // 幂等：再次调用不改变已有字段
        let hint = tagged["recovery_hint"].clone();
        enrich_error(&mut tagged);
        assert_eq!(tagged["error_code"], "CUSTOM");
        assert_eq!(tagged["recovery_hint"], hint);
    }

    #[test]
    fn enrich_fills_ux_fields_for_bridge_injected_code() {
        // bridge 出口只注入 error_code，本层补齐 hint/retryable
        let mut v =
            json!({"ok": false, "error": "recv: connection lost", "error_code": "CONNECTION_LOST"});
        enrich_error(&mut v);
        assert_eq!(v["error_code"], "CONNECTION_LOST");
        assert!(v["recovery_hint"].as_str().unwrap().contains("重试"));
        assert_eq!(v["retryable"], true);
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

        let r = classify_message("forbidden: 'list_recordings' requires superadmin");
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

        let r = classify_message("NOT IN YOUR GROUP");
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
