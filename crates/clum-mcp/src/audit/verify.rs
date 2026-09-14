//! Audit hash-chain verification.
//!
//! 只读校验：按 id 升序重算全链，检测任何历史篡改/删除。
//! 合法断点仅有两类：prev_hash 为 NULL（创世/迁移跨代锚）；
//! prev_hash 命中 cleanup 记录的 checkpoint（管理性前缀清理）。
//! 对被篡改的数据（含非 hex 垃圾的 prev_hash）一律报告 BROKEN，绝不 panic。

use crate::audit::chain::{self, ChainRow};
use crate::audit::AuditDb;
use rusqlite::Connection;
use std::collections::HashSet;

#[derive(Debug)]
pub struct ChainReport {
    pub ok: bool,
    /// entry_hash 非空的行数（参与校验）
    pub hashed_rows: u64,
    /// 迁移前旧行数（entry_hash 为 NULL，不参与校验）
    pub pre_hash_rows: u64,
    /// 合法断点数（创世 + checkpoints）
    pub segments: u64,
    /// 最新哈希行的 entry_hash（供外置监控比对）
    pub chain_head: Option<String>,
    /// 首个断点：(id, expected, actual)
    pub first_failure: Option<(i64, String, String)>,
}

/// 同步核心：调用方需持有 conn 锁（与 log/cleanup 同模式）；
/// 直接借用 &Connection，不二次 lock（std Mutex 不可重入）。
pub(crate) fn verify_conn(conn: &Connection) -> ChainReport {
    let mut report = ChainReport {
        ok: true,
        hashed_rows: 0,
        pre_hash_rows: 0,
        segments: 0,
        chain_head: None,
        first_failure: None,
    };

    // 预载 checkpoint 集合（管理断点白名单）。表不存在 → 空集兜底。
    let checkpoints: HashSet<String> = conn
        .prepare(
            "SELECT last_deleted_entry_hash FROM audit_chain_checkpoints
             WHERE last_deleted_entry_hash IS NOT NULL",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |r| r.get::<_, String>(0))
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();

    let mut stmt = match conn.prepare(
        "SELECT id, event_id, timestamp, agent_name, host_name, session_name,
                pane_id, operation_id, action, detail, redacted, output_summary,
                success, duration_ms, error_message, prev_hash, entry_hash
         FROM audit_events ORDER BY id ASC",
    ) {
        Ok(s) => s,
        Err(_) => {
            report.ok = false;
            report.first_failure = Some((0, "table query failed".into(), String::new()));
            return report;
        }
    };

    let mut rows_iter = match stmt.query([]) {
        Ok(it) => it,
        Err(_) => {
            report.ok = false;
            report.first_failure = Some((0, "query failed".into(), String::new()));
            return report;
        }
    };

    let mut last_entry_hash: Option<String> = None;
    while let Ok(Some(r)) = rows_iter.next() {
        // 列序（0 基）：id=0 event_id=1 ... error_message=14 prev_hash=15 entry_hash=16
        let stored_entry: Option<String> = r.get(16).ok();
        let Some(stored) = stored_entry else {
            report.pre_hash_rows += 1;
            continue;
        };

        report.hashed_rows += 1;
        let id: i64 = r.get(0).unwrap_or(0);
        let prev_hash: Option<String> = r.get(15).ok();

        // 格式预判（必须在重算之前）：被篡改的库可能存非 hex 垃圾进
        // prev_hash 列；verify 必须把恶意数据当 BROKEN 报告，绝不 panic
        // （chain::entry_hash 内部 hex decode 对非法输入会 panic）。
        if let Some(p) = &prev_hash {
            if p.len() != 64 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
                report.ok = false;
                report.first_failure = Some((id, "<malformed prev_hash>".into(), p.clone()));
                return report;
            }
        }

        // 重算：payload 由行值重建，编码与写入侧共用同一 entry_hash。
        let row = ChainRow {
            event_id: r.get(1).unwrap_or_default(),
            timestamp: r.get(2).unwrap_or_default(),
            agent_name: r.get(3).unwrap_or_default(),
            host_name: r.get(4).unwrap_or_default(),
            session_name: r.get(5).unwrap_or_default(),
            pane_id: r.get(6).ok(),
            operation_id: r.get(7).ok(),
            action: r.get(8).unwrap_or_default(),
            detail: r.get(9).unwrap_or_default(),
            redacted: r.get::<_, i64>(10).map(|v| v != 0).unwrap_or(false),
            output_summary: r.get(11).ok(),
            success: r.get::<_, i64>(12).map(|v| v != 0).unwrap_or(false),
            duration_ms: r.get(13).unwrap_or(0),
            error_message: r.get(14).ok(),
        };
        let prev_input = prev_hash
            .clone()
            .unwrap_or_else(|| chain::GENESIS_PREV.to_string());
        let recomputed = chain::entry_hash(&prev_input, &row);
        if recomputed != stored {
            report.ok = false;
            report.first_failure = Some((id, recomputed, stored));
            return report;
        }

        match &prev_hash {
            None => report.segments += 1, // 创世 / 迁移跨代锚
            Some(p) => {
                let continues = last_entry_hash.as_deref() == Some(p.as_str());
                let checkpoint = checkpoints.contains(p);
                if continues || checkpoint {
                    if checkpoint && !continues {
                        report.segments += 1; // 管理性清理断点
                    }
                } else {
                    report.ok = false;
                    report.first_failure = Some((
                        id,
                        last_entry_hash.clone().unwrap_or_else(|| "<none>".into()),
                        p.clone(),
                    ));
                    return report;
                }
            }
        }
        last_entry_hash = Some(stored);
    }
    report.chain_head = last_entry_hash;
    report
}

impl AuditDb {
    /// 异步包装：全链校验（只读）。
    pub async fn verify_chain(&self) -> anyhow::Result<ChainReport> {
        let db = self.conn_ref().clone();
        let report = tokio::task::spawn_blocking(move || {
            let conn = db.lock().unwrap_or_else(|e| e.into_inner());
            Ok::<_, anyhow::Error>(verify_conn(&conn))
        })
        .await??;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditDb;
    use chrono::Utc;
    use clum_core::types::{AuditAction, AuditEvent};
    use uuid::Uuid;

    async fn log(db: &AuditDb, detail: &str) {
        db.log(AuditEvent {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            agent_name: "t".into(),
            host_name: "tf01".into(),
            session_name: "clum".into(),
            pane_id: None,
            operation_id: None,
            action: AuditAction::Exec,
            detail: detail.into(),
            redacted: false,
            output_summary: None,
            success: true,
            duration_ms: 0,
            error_message: None,
        })
        .await;
    }

    fn conn(db: &AuditDb) -> std::sync::MutexGuard<'_, Connection> {
        db.conn_ref().lock().unwrap_or_else(|e| e.into_inner())
    }

    #[tokio::test]
    async fn test_verify_ok_and_head() {
        let db = AuditDb::open_in_memory().unwrap();
        for i in 0..5 {
            log(&db, &format!("e{i}")).await;
        }
        let report = db.verify_chain().await.unwrap();
        assert!(report.ok);
        assert_eq!(report.hashed_rows, 5);
        assert_eq!(report.segments, 1);
        assert!(report.chain_head.is_some());
    }

    #[tokio::test]
    async fn test_detects_value_tamper() {
        let db = AuditDb::open_in_memory().unwrap();
        log(&db, "a").await;
        log(&db, "b").await;
        conn(&db)
            .execute("UPDATE audit_events SET detail='tampered' WHERE id=1", [])
            .unwrap();
        let report = db.verify_chain().await.unwrap();
        assert!(!report.ok);
        let (id, _, _) = report.first_failure.unwrap();
        assert_eq!(id, 1);
    }

    #[tokio::test]
    async fn test_detects_middle_deletion() {
        let db = AuditDb::open_in_memory().unwrap();
        for i in 0..3 {
            log(&db, &format!("e{i}")).await;
        }
        conn(&db)
            .execute("DELETE FROM audit_events WHERE id=2", [])
            .unwrap();
        // 行3的 prev_hash 指向已删行2，断链
        let report = db.verify_chain().await.unwrap();
        assert!(!report.ok);
    }

    #[tokio::test]
    async fn test_malformed_prev_hash_reports_broken_not_panic() {
        let db = AuditDb::open_in_memory().unwrap();
        log(&db, "a").await;
        log(&db, "b").await;
        conn(&db)
            .execute(
                "UPDATE audit_events SET prev_hash='deadbeef' WHERE id=2",
                [],
            )
            .unwrap();
        // 不得 panic；被篡改数据 = BROKEN
        let report = db.verify_chain().await.unwrap();
        assert!(!report.ok);
        let (id, expected, _) = report.first_failure.unwrap();
        assert_eq!(id, 2);
        assert_eq!(expected, "<malformed prev_hash>");
    }

    #[tokio::test]
    async fn test_legacy_rows_then_chain_treated_as_two_generations() {
        let db = AuditDb::open_in_memory().unwrap();
        // 旧行（无哈希）——绕过 log()，模拟迁移前数据
        conn(&db)
            .execute(
                "INSERT INTO audit_events (event_id, timestamp, agent_name, host_name, session_name, action, detail, success)
                 VALUES ('legacy-1', '2020-01-01T00:00:00Z', 'old', 'tf01', 'clum', 'Exec', 'old world', 1)",
                [],
            )
            .unwrap();
        // log() 取不到 prev → prev_hash=NULL → 新链创世锚
        log(&db, "new gen").await;
        let report = db.verify_chain().await.unwrap();
        assert!(report.ok);
        assert_eq!(report.pre_hash_rows, 1);
        assert_eq!(report.hashed_rows, 1);
        assert_eq!(report.segments, 1);
    }
}
