//! Hash-chain primitives for the audit log. Pure functions — no IO.
//!
//! `entry_hash = SHA256(prev_hash_bytes || payload(row))`，payload 为 14 个
//! 值列的长度前缀串接。编码刻意不经过 serde：避免结构体演进/特性开关
//! 改变序列化字节造成断链。写入侧（log.rs）与校验侧（verify.rs）共用
//! 同一 `ChainRow` 与 `entry_hash`，保证两侧输入逐字节一致。
//!
//! decode 失败即 panic 的语义仅对写入路径成立（prev 恒为本函数产出或
//! GENESIS）；校验路径必须在调用前做格式预判（见 verify.rs）。

use clum_core::types::AuditEvent;
use sha2::{Digest, Sha256};

/// 创世前值：全零 32 字节的 hex。库中首条哈希行 prev_hash 列存 NULL。
pub(crate) const GENESIS_PREV: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// 与 `audit_events` 的 14 个值列一一对应的行值快照。
/// action 必须经过与 INSERT 相同的变体名变换（见 `action_column`）。
pub(crate) struct ChainRow {
    pub event_id: String,
    pub timestamp: String,
    pub agent_name: String,
    pub host_name: String,
    pub session_name: String,
    pub pane_id: Option<String>,
    pub operation_id: Option<String>,
    pub action: String,
    pub detail: String,
    pub redacted: bool,
    pub output_summary: Option<String>,
    pub success: bool,
    pub duration_ms: i64,
    pub error_message: Option<String>,
}

impl ChainRow {
    /// 从 AuditEvent 构建入库行值。字段变换必须与 log.rs 的 INSERT 列一致。
    pub(crate) fn from_event(event: &AuditEvent) -> Self {
        Self {
            event_id: event.event_id.to_string(),
            timestamp: event.timestamp.to_rfc3339(),
            agent_name: event.agent_name.clone(),
            host_name: event.host_name.clone(),
            session_name: event.session_name.clone(),
            pane_id: event.pane_id.clone(),
            operation_id: event.operation_id.clone(),
            action: action_column(&event.action),
            detail: event.detail.clone(),
            redacted: event.redacted,
            output_summary: event.output_summary.clone(),
            success: event.success,
            duration_ms: event.duration_ms as i64,
            error_message: event.error_message.clone(),
        }
    }
}

/// action 列的入库变换：serde 变体名、去除 JSON 引号。log.rs 的 INSERT
/// 与 ChainRow::from_event 都必须调用本函数，保证哈希输入 = 库中值。
pub(crate) fn action_column(action: &clum_core::types::AuditAction) -> String {
    serde_json::to_string(action)
        .unwrap_or_else(|e| {
            tracing::error!("failed to serialize audit action: {}", e);
            format!("{action:?}")
        })
        .trim_matches('"')
        .to_string()
}

fn append_str(hasher: &mut Sha256, s: &str) {
    hasher.update((s.len() as u64).to_le_bytes());
    hasher.update(s.as_bytes());
}

fn append_opt_str(hasher: &mut Sha256, v: Option<&str>) {
    match v {
        None => hasher.update([0xFF; 8]),
        Some(s) => append_str(hasher, s),
    }
}

fn append_bool(hasher: &mut Sha256, b: bool) {
    hasher.update([b as u8]);
}

fn append_i64(hasher: &mut Sha256, v: i64) {
    hasher.update(v.to_le_bytes());
}

fn prev_bytes(prev: &str) -> [u8; 32] {
    // 仅写入路径调用（prev 恒为合法 hex）；verify 侧先做格式预判再进本函数。
    let mut out = [0u8; 32];
    hex::decode_to_slice(prev, &mut out).expect("prev_hash must be 64-char hex");
    out
}

/// 计算一条记录的 entry_hash。
pub(crate) fn entry_hash(prev: &str, row: &ChainRow) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_bytes(prev));
    append_str(&mut hasher, &row.event_id);
    append_str(&mut hasher, &row.timestamp);
    append_str(&mut hasher, &row.agent_name);
    append_str(&mut hasher, &row.host_name);
    append_str(&mut hasher, &row.session_name);
    append_opt_str(&mut hasher, row.pane_id.as_deref());
    append_opt_str(&mut hasher, row.operation_id.as_deref());
    append_str(&mut hasher, &row.action);
    append_str(&mut hasher, &row.detail);
    append_bool(&mut hasher, row.redacted);
    append_opt_str(&mut hasher, row.output_summary.as_deref());
    append_bool(&mut hasher, row.success);
    append_i64(&mut hasher, row.duration_ms);
    append_opt_str(&mut hasher, row.error_message.as_deref());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(agent: &str, host: &str) -> ChainRow {
        ChainRow {
            event_id: "evt-1".into(),
            timestamp: "2026-09-14T00:00:00+00:00".into(),
            agent_name: agent.into(),
            host_name: host.into(),
            session_name: "clum".into(),
            pane_id: Some("%0".into()),
            operation_id: None,
            action: "Exec".into(),
            detail: "systemctl status nginx".into(),
            redacted: false,
            output_summary: Some("ok".into()),
            success: true,
            duration_ms: 42,
            error_message: None,
        }
    }

    #[test]
    fn test_deterministic() {
        let r = sample_row("a", "h");
        let h1 = entry_hash(GENESIS_PREV, &r);
        let h2 = entry_hash(GENESIS_PREV, &r);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn test_prev_changes_hash() {
        let r = sample_row("a", "h");
        assert_ne!(
            entry_hash(GENESIS_PREV, &r),
            entry_hash("ff".repeat(32).as_str(), &r)
        );
    }

    #[test]
    fn test_field_boundary_no_concatenation_ambiguity() {
        // 无长度前缀时 ("ab","cd") 与 ("abcd","") 的朴素串接字节流相同；
        // 长度前缀必须使二者哈希不同——防字段边界拼接歧义攻击。
        let a = entry_hash(GENESIS_PREV, &sample_row("ab", "cd"));
        let b = entry_hash(GENESIS_PREV, &sample_row("abcd", ""));
        assert_ne!(a, b);
    }

    #[test]
    fn test_option_none_vs_empty_distinct() {
        let mut r1 = sample_row("a", "h");
        r1.pane_id = None;
        let mut r2 = sample_row("a", "h");
        r2.pane_id = Some(String::new());
        assert_ne!(entry_hash(GENESIS_PREV, &r1), entry_hash(GENESIS_PREV, &r2));
    }
}
