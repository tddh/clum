use crate::audit::verify::verify_conn;
use crate::audit::AuditDb;
use anyhow::Result;

impl AuditDb {
    /// Cleanup old records by time threshold (retention_days) and file size
    /// (max_size_mb). Either DELETE that removes hash-chained rows records a
    /// checkpoint FIRST, so `audit verify` treats the resulting chain break
    /// as an authorized management deletion instead of tampering.
    pub async fn cleanup(&self, retention_days: u32, max_size_mb: u64) -> Result<()> {
        let db = self.conn_ref().clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = db.lock().unwrap_or_else(|e| e.into_inner());

            // 1. Time cleanup. Probe the chain tail BEFORE DELETE (the value
            //    vanishes with the rows); predicate must match the DELETE
            //    below or checkpoints record a stale hash.
            let expiry = format!("-{} days", retention_days);
            let probe: Option<String> = conn
                .query_row(
                    "SELECT entry_hash FROM audit_events
                     WHERE timestamp < datetime('now', ?1) AND entry_hash IS NOT NULL
                     ORDER BY id DESC LIMIT 1",
                    rusqlite::params![expiry],
                    |r| r.get(0),
                )
                .ok();
            let time_deleted = conn.execute(
                "DELETE FROM audit_events WHERE timestamp < datetime('now', ?1)",
                rusqlite::params![expiry],
            )?;
            if time_deleted > 0 {
                tracing::info!("audit cleanup: removed {time_deleted} expired records");
            }
            if time_deleted > 0 && probe.is_some() {
                conn.execute(
                    "INSERT INTO audit_chain_checkpoints
                        (last_deleted_entry_hash, first_remaining_id)
                     VALUES (?1, (SELECT MIN(id) FROM audit_events))",
                    rusqlite::params![probe],
                )?;
            }

            // 2. Size cleanup: oldest 10% past the file-size cap. In-memory
            //    DBs have no file metadata — branch silently skipped.
            //    ⚠️ No automated test coverage (unreachable in-memory);
            //    mirrors path 1 structurally.
            let db_path = conn.path().unwrap_or("audit.db");
            if let Ok(meta) = std::fs::metadata(db_path) {
                let file_size_mb = meta.len() / (1024 * 1024);
                if file_size_mb > max_size_mb {
                    let size_where = "id IN (
                        SELECT id FROM audit_events ORDER BY timestamp ASC
                        LIMIT (SELECT CAST(COUNT(*) * 0.1 AS INTEGER)
                               FROM audit_events))";
                    let probe: Option<String> = conn
                        .query_row(
                            &format!(
                                "SELECT entry_hash FROM audit_events
                                 WHERE {size_where} AND entry_hash IS NOT NULL
                                 ORDER BY id DESC LIMIT 1"
                            ),
                            [],
                            |r| r.get(0),
                        )
                        .ok();
                    let size_deleted =
                        conn.execute(&format!("DELETE FROM audit_events WHERE {size_where}"), [])?;
                    if size_deleted > 0 {
                        tracing::info!(
                            "audit cleanup: removed {size_deleted} records \
                             (size threshold: {}MB > {}MB)",
                            file_size_mb,
                            max_size_mb
                        );
                    }
                    if size_deleted > 0 && probe.is_some() {
                        conn.execute(
                            "INSERT INTO audit_chain_checkpoints
                                (last_deleted_entry_hash, first_remaining_id)
                             VALUES (?1, (SELECT MIN(id) FROM audit_events))",
                            rusqlite::params![probe],
                        )?;
                    }
                }
            }

            // 3. Post-hoc self-check: cleanup must never break the chain. A
            //    failure means non-prefix deletion (clock skew); the data is
            //    already gone — warn loudly. Borrows the held &conn directly
            //    (std Mutex is not reentrant — a second lock would deadlock).
            let report = verify_conn(&conn);
            if !report.ok {
                tracing::warn!(
                    "audit cleanup: hash-chain self-check FAILED at id={:?} \
                     — non-prefix deletion detected; chain integrity compromised",
                    report.first_failure.map(|f| f.0)
                );
            }

            conn.execute_batch("PRAGMA optimize;")?;
            Ok(())
        })
        .await??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditDb;
    use chrono::{TimeZone, Utc};
    use clum_core::types::{AuditAction, AuditEvent};
    use uuid::Uuid;

    async fn log_at(db: &AuditDb, detail: &str, ts: chrono::DateTime<Utc>) {
        db.log(AuditEvent {
            event_id: Uuid::new_v4(),
            timestamp: ts,
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

    fn ts(y: i32, mo: u32, d: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, 0, 0, 0).unwrap()
    }

    fn checkpoint_count(db: &AuditDb) -> i64 {
        let conn = db.conn_ref().lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row("SELECT COUNT(*) FROM audit_chain_checkpoints", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    #[tokio::test]
    async fn test_time_cleanup_leaves_checkpoint_and_verifiable_suffix() {
        let db = AuditDb::open_in_memory().unwrap();
        log_at(&db, "old-1", ts(2020, 1, 1)).await;
        log_at(&db, "keep-1", ts(2099, 1, 1)).await; // 未来时间不被清理

        db.cleanup(90, 500).await.unwrap();

        assert!(
            checkpoint_count(&db) >= 1,
            "删除哈希行时必须记录 checkpoint"
        );
        let report = db.verify_chain().await.unwrap();
        assert!(report.ok, "保留段必须仍可校验: {:?}", report.first_failure);
        assert_eq!(report.hashed_rows, 1);
    }

    #[tokio::test]
    async fn test_cleanup_then_new_writes_continue_seamlessly() {
        let db = AuditDb::open_in_memory().unwrap();
        log_at(&db, "old-1", ts(2020, 1, 1)).await;
        log_at(&db, "keep-1", ts(2099, 1, 1)).await;

        db.cleanup(90, 500).await.unwrap();
        log_at(&db, "post-1", ts(2099, 1, 2)).await;

        let report = db.verify_chain().await.unwrap();
        assert!(
            report.ok,
            "清理后新写入必须整链可校验: {:?}",
            report.first_failure
        );
        assert_eq!(report.hashed_rows, 2);
        assert_eq!(
            report.segments, 1,
            "keep-1 经 checkpoint 承认为段首；post-1 无缝相接不增段"
        );
    }

    #[tokio::test]
    async fn test_untouched_cleanup_records_no_checkpoint() {
        let db = AuditDb::open_in_memory().unwrap();
        log_at(&db, "keep-1", ts(2099, 1, 1)).await;
        db.cleanup(90, 500).await.unwrap();
        assert_eq!(
            checkpoint_count(&db),
            0,
            "未删除任何行时不得产生 checkpoint"
        );
    }
}
