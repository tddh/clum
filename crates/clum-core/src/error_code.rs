//! 共享错误码分类：错误消息 → 稳定 error_code 字符串。
//!
//! 由 clum-mcp（错误信封分类）与 rmux-bridge（响应出口注入 error_code）共同使用，
//! 保证两端的错误码分类永远一致。只返回 code，不返回 recovery_hint / retryable——
//! 那些是 MCP 侧 UX 层关心的事，由 clum-mcp/src/error.rs 的本地映射表提供。
//!
//! 设计原则：错误码**只增不改**。`error` 保留原始字符串（向后兼容），
//! `error_code` 是稳定契约，AI 客户端凭它可靠分支。

/// 未分类兜底
pub const CODE_UNKNOWN: &str = "UNKNOWN";
/// 分组隔离拒绝（API Key 无权限访问目标主机）
pub const CODE_FORBIDDEN: &str = "FORBIDDEN";
/// 主机不在注册表
pub const CODE_HOST_NOT_FOUND: &str = "HOST_NOT_FOUND";
/// 缺少/非法参数
pub const CODE_INVALID_PARAMS: &str = "INVALID_PARAMS";
/// 会话不存在 / 已存在
pub const CODE_SESSION_NOT_FOUND: &str = "SESSION_NOT_FOUND";
pub const CODE_SESSION_EXISTS: &str = "SESSION_EXISTS";
/// pane 不存在/无效 / 非空闲
pub const CODE_PANE_NOT_FOUND: &str = "PANE_NOT_FOUND";
pub const CODE_PANE_BUSY: &str = "PANE_BUSY";
/// 窗口不存在
pub const CODE_WINDOW_NOT_FOUND: &str = "WINDOW_NOT_FOUND";
/// 隧道不存在 / 目标不在白名单
pub const CODE_FORWARD_NOT_FOUND: &str = "FORWARD_NOT_FOUND";
pub const CODE_FORWARD_DENIED: &str = "FORWARD_DENIED";
/// 路径穿越/不安全路径
pub const CODE_PATH_TRAVERSAL: &str = "PATH_TRAVERSAL";
/// 直连模式认证失败
pub const CODE_AUTH_FAILED: &str = "AUTH_FAILED";
/// bridge 不可达（拒绝/注册失败）
pub const CODE_BRIDGE_UNREACHABLE: &str = "BRIDGE_UNREACHABLE";
/// 连接中断
pub const CODE_CONNECTION_LOST: &str = "CONNECTION_LOST";
/// 连接超时（可重试）
pub const CODE_CONNECT_TIMEOUT: &str = "CONNECT_TIMEOUT";
/// 命令执行/等待超时（不可盲目重试）
pub const CODE_TIMEOUT: &str = "TIMEOUT";
/// bridge 端 rmux CLI 回退失败
pub const CODE_CLI_FAILED: &str = "CLI_FAILED";
/// 帧协议错误（不应发生）
pub const CODE_PROTOCOL_ERROR: &str = "PROTOCOL_ERROR";

/// 按错误消息分类为稳定 error_code。匹配顺序即优先级：更具体的模式在前。
///
/// 大小写不敏感，基于子串匹配。此函数被 MCP 与 bridge 共用，新增匹配模式时
/// 必须保持两侧行为一致（clum-mcp 的 hint/retryable 表会为每个 code 补充 UX 字段）。
pub fn classify_error_message(msg: &str) -> &'static str {
    let m = msg.to_lowercase();
    let has = |p: &str| m.contains(p);

    // ── RBAC / 分组隔离 ──
    if has("not in your group") || has("forbidden") {
        return CODE_FORBIDDEN;
    }
    // ── 主机 ──
    if has("host not found") || has("not found in enrolled") {
        return CODE_HOST_NOT_FOUND;
    }
    // ── 参数缺失 ──
    if has("missing '") || has("empty hosts list") {
        return CODE_INVALID_PARAMS;
    }
    // ── pane ──
    // bridge 最高频错误：invalid pane_id / invalid source_pane_id / invalid target_pane_id
    if has("invalid pane") || has("invalid source_pane") || has("invalid target_pane") {
        return CODE_PANE_NOT_FOUND;
    }
    if (has("pane id") && has("not found"))
        || has("can't find pane")
        || has("pane not found")
        || has("no pane found with title")
        || has("pane has no id")
    {
        return CODE_PANE_NOT_FOUND;
    }
    // ── session ──
    if has("session already exists") || has("duplicate session") {
        return CODE_SESSION_EXISTS;
    }
    if has("session not found") || has("can't find session") || has("no such session") {
        return CODE_SESSION_NOT_FOUND;
    }
    // ── window ──
    if has("window not found") || has("can't find window") {
        return CODE_WINDOW_NOT_FOUND;
    }
    // ── forward ──
    if has("forward not found") {
        return CODE_FORWARD_NOT_FOUND;
    }
    // ── pane 非空闲 ──
    if has("pane still active") {
        return CODE_PANE_BUSY;
    }
    // ── 路径安全 ──
    if has("path traversal")
        || has("unsafe relative path")
        || has("null byte")
        || has("directory too deep")
    {
        return CODE_PATH_TRAVERSAL;
    }
    // ── 转发白名单 ──
    if has("not in allowed list") {
        return CODE_FORWARD_DENIED;
    }
    // ── 认证（direct 模式）──
    if has("authentication failed")
        || has("auth failed")
        || has("invalid auth preamble")
        || has("token too long")
    {
        return CODE_AUTH_FAILED;
    }
    // ── 连接失败 ──
    if has("connection refused") || has("registration rejected") || has("handshake failed") {
        return CODE_BRIDGE_UNREACHABLE;
    }
    if has("connection lost") || has("connection reset") {
        return CODE_CONNECTION_LOST;
    }
    // ── 连接超时（必须在通用 timeout 之前）──
    if has("connect timeout") || has("connect timed out") || has("connection timed out") {
        return CODE_CONNECT_TIMEOUT;
    }
    // ── 执行/等待超时 ──
    if has("timeout") || has("timed out") {
        return CODE_TIMEOUT;
    }
    // ── 参数值域（锚定模式，避免宽泛子串误伤未来新增的状态类错误）──
    if has("invalid direction")
        || has("must be 0-65535")
        || has("must be non-zero")
        || has("must be positive")
        || has("must be absolute")
        || has("mutually exclusive")
        || has("coordinates")
        || has("base64")
        || has("unknown layout")
    {
        return CODE_INVALID_PARAMS;
    }
    // ── bridge 端 rmux CLI 回退失败 ──
    if has("rmux cli") || has("cli command") {
        return CODE_CLI_FAILED;
    }
    // ── 帧协议错误 ──
    if has("frame too large") || has("unknown request type") || has("invalid json") {
        return CODE_PROTOCOL_ERROR;
    }
    CODE_UNKNOWN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_registry_and_params() {
        assert_eq!(
            classify_error_message("host not found: tf99"),
            CODE_HOST_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("host 'tf99' not found in enrolled bridges"),
            CODE_HOST_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("missing 'pane_id'"),
            CODE_INVALID_PARAMS
        );
        assert_eq!(
            classify_error_message("empty hosts list"),
            CODE_INVALID_PARAMS
        );
        assert_eq!(
            classify_error_message("invalid direction: up"),
            CODE_INVALID_PARAMS
        );
        assert_eq!(
            classify_error_message("cols must be 0-65535"),
            CODE_INVALID_PARAMS
        );
    }

    #[test]
    fn classifies_terminal_objects() {
        assert_eq!(
            classify_error_message("invalid pane_id: %99"),
            CODE_PANE_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("invalid source_pane_id: %1"),
            CODE_PANE_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("pane id %99 was not found"),
            CODE_PANE_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("pane not found in info snapshot"),
            CODE_PANE_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("no pane found with title: foo"),
            CODE_PANE_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("split pane has no id"),
            CODE_PANE_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("session not found: clum"),
            CODE_SESSION_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("session already exists: clum"),
            CODE_SESSION_EXISTS
        );
        assert_eq!(
            classify_error_message("window not found"),
            CODE_WINDOW_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("forward not found: abc"),
            CODE_FORWARD_NOT_FOUND
        );
        assert_eq!(classify_error_message("pane still active"), CODE_PANE_BUSY);
    }

    #[test]
    fn classifies_security_denials() {
        assert_eq!(
            classify_error_message("path traversal rejected: ../../etc"),
            CODE_PATH_TRAVERSAL
        );
        assert_eq!(
            classify_error_message("path contains null byte"),
            CODE_PATH_TRAVERSAL
        );
        assert_eq!(
            classify_error_message("directory too deep (>64)"),
            CODE_PATH_TRAVERSAL
        );
        assert_eq!(
            classify_error_message("forward target 10.0.0.1:22 not in allowed list"),
            CODE_FORWARD_DENIED
        );
        assert_eq!(
            classify_error_message("bridge QUIC authentication failed"),
            CODE_AUTH_FAILED
        );
        assert_eq!(
            classify_error_message("host tf01 not in your group"),
            CODE_FORBIDDEN
        );
        assert_eq!(
            classify_error_message("forbidden: requires superadmin"),
            CODE_FORBIDDEN
        );
    }

    #[test]
    fn classifies_network_conditions() {
        assert_eq!(
            classify_error_message("connection refused"),
            CODE_BRIDGE_UNREACHABLE
        );
        assert_eq!(
            classify_error_message("QUIC handshake failed"),
            CODE_BRIDGE_UNREACHABLE
        );
        assert_eq!(
            classify_error_message("registration rejected: bad token"),
            CODE_BRIDGE_UNREACHABLE
        );
        assert_eq!(
            classify_error_message("recv: connection lost"),
            CODE_CONNECTION_LOST
        );
        assert_eq!(
            classify_error_message("QUIC connect timeout"),
            CODE_CONNECT_TIMEOUT
        );
        assert_eq!(
            classify_error_message("TCP connect timed out"),
            CODE_CONNECT_TIMEOUT
        );
        // 连接超时必须优先于通用 timeout
        assert_eq!(
            classify_error_message("connection timed out"),
            CODE_CONNECT_TIMEOUT
        );
        assert_eq!(
            classify_error_message("timeout waiting for sentinel after 15000ms"),
            CODE_TIMEOUT
        );
        assert_eq!(
            classify_error_message("timeout waiting for: root@"),
            CODE_TIMEOUT
        );
    }

    #[test]
    fn classifies_cli_and_protocol() {
        assert_eq!(
            classify_error_message("rmux CLI failed: ls: no such file"),
            CODE_CLI_FAILED
        );
        assert_eq!(
            classify_error_message("CLI command 'clear-history' exited with code 2: err"),
            CODE_CLI_FAILED
        );
        assert_eq!(
            classify_error_message("frame too large: 999999 bytes"),
            CODE_PROTOCOL_ERROR
        );
        assert_eq!(
            classify_error_message("unknown request type: foo"),
            CODE_PROTOCOL_ERROR
        );
        assert_eq!(
            classify_error_message("invalid json: expected value"),
            CODE_PROTOCOL_ERROR
        );
    }

    #[test]
    fn fallback_is_unknown() {
        assert_eq!(classify_error_message("something unexpected"), CODE_UNKNOWN);
        assert_eq!(classify_error_message(""), CODE_UNKNOWN);
        // 锚定后的 must be 模式不误伤状态类消息（宽泛 must be 曾会误分类）
        assert_eq!(
            classify_error_message("session must be created first"),
            CODE_UNKNOWN
        );
        assert_eq!(
            classify_error_message("pane must be closed before respawn"),
            CODE_UNKNOWN
        );
    }

    #[test]
    fn classifies_case_insensitive() {
        assert_eq!(
            classify_error_message("HOST NOT FOUND: tf99"),
            CODE_HOST_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("Connection REFUSED"),
            CODE_BRIDGE_UNREACHABLE
        );
        assert_eq!(
            classify_error_message("INVALID PANE_ID: %0"),
            CODE_PANE_NOT_FOUND
        );
        assert_eq!(
            classify_error_message("QUIC CONNECT TIMEOUT"),
            CODE_CONNECT_TIMEOUT
        );
    }
}
