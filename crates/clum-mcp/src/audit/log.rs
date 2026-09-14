use crate::audit::chain::{self, ChainRow, GENESIS_PREV};
use crate::audit::AuditDb;
use clum_core::types::AuditEvent;
use rusqlite::params;

impl AuditDb {
    /// Async wrapper: logs an audit event via spawn_blocking.
    /// Failures are silently traced — never blocks the caller.
    pub async fn log(&self, event: AuditEvent) {
        let db = self.conn_ref().clone();
        let result = tokio::task::spawn_blocking(move || {
            let conn = db.lock().unwrap_or_else(|e| e.into_inner());

            // 链前值：库中最新一条哈希行的 entry_hash；无则创世。
            // 单连接 + Mutex 串行化保证 SELECT 与 INSERT 之间无并发写者。
            let prev_hash: Option<String> = conn
                .query_row(
                    "SELECT entry_hash FROM audit_events
                     WHERE entry_hash IS NOT NULL
                     ORDER BY id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .ok(); // 无哈希行 → 创世
            let prev_input =
                prev_hash.clone().unwrap_or_else(|| GENESIS_PREV.to_string());

            let row = ChainRow::from_event(&event);
            let entry = chain::entry_hash(&prev_input, &row);

            conn.execute(
                "INSERT INTO audit_events
                    (event_id, timestamp, agent_name, host_name, session_name,
                     pane_id, operation_id, action, detail, redacted, output_summary,
                     success, duration_ms, error_message, prev_hash, entry_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    row.event_id,
                    row.timestamp,
                    row.agent_name,
                    row.host_name,
                    row.session_name,
                    row.pane_id,
                    row.operation_id,
                    row.action,
                    row.detail,
                    row.redacted as i32,
                    row.output_summary,
                    row.success as i32,
                    row.duration_ms,
                    row.error_message,
                    prev_hash, // 创世行存 NULL，verify 据此识别合法锚
                    entry,
                ],
            )
        })
        .await;

        match result {
            Ok(Ok(_)) => {} // success, silent
            Ok(Err(e)) => {
                tracing::error!("audit write failed: {e}");
            }
            Err(join_err) => {
                tracing::error!("audit spawn_blocking panic: {join_err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use clum_core::types::{AuditAction, AuditEvent};
    use uuid::Uuid;

    fn ev(detail: &str) -> AuditEvent {
        AuditEvent {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            agent_name: "t".into(),
            host_name: "tf01".into(),
            session_name: "clum".into(),
            pane_id: Some("%0".into()),
            operation_id: None,
            action: AuditAction::Exec,
            detail: detail.into(),
            redacted: false,
            output_summary: None,
            success: true,
            duration_ms: 1,
            error_message: None,
        }
    }

    fn last_rows(db: &AuditDb) -> Vec<(i64, Option<String>, Option<String>)> {
        let conn = db.conn_ref().lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare("SELECT id, prev_hash, entry_hash FROM audit_events ORDER BY id ASC")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[tokio::test]
    async fn test_chain_links_consecutive_events() {
        let db = crate::audit::AuditDb::open_in_memory().unwrap();
        db.log(ev("first")).await;
        db.log(ev("second")).await;
        db.log(ev("third")).await;

        let rows = last_rows(&db);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].1, None, "首条哈希行 prev_hash 必须为 NULL（创世）");
        assert!(rows[0].2.is_some());
        for i in 1..rows.len() {
            assert_eq!(
                rows[i].1.as_deref(),
                rows[i - 1].2.as_deref(),
                "行 {} 的 prev_hash 必须等于上一行 entry_hash",
                i
            );
        }
    }

    #[tokio::test]
    async fn test_concurrent_logs_keep_chain_unbroken() {
        let db = std::sync::Arc::new(crate::audit::AuditDb::open_in_memory().unwrap());
        let mut handles = Vec::new();
        for _ in 0..10 {
            let d = db.clone();
            handles.push(tokio::spawn(async move {
                d.log(ev("concurrent")).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let rows = last_rows(&db);
        assert_eq!(rows.len(), 10);
        for i in 1..rows.len() {
            assert_eq!(rows[i].1.as_deref(), rows[i - 1].2.as_deref());
        }
    }
}
