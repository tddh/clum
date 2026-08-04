use crate::audit::AuditDb;
use anyhow::Result;

impl AuditDb {
    /// Cleanup old records by time threshold (retention_days) and file size (max_size_mb).
    /// Whichever threshold triggers first gets applied.
    pub async fn cleanup(&self, retention_days: u32, max_size_mb: u64) -> Result<()> {
        let db = self.conn_ref().clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = db.lock().unwrap_or_else(|e| e.into_inner());

            // 1. Time cleanup: remove records older than retention_days
            let time_deleted = conn.execute(
                "DELETE FROM audit_events WHERE timestamp < datetime('now', ?1)",
                rusqlite::params![format!("-{} days", retention_days)],
            )?;
            if time_deleted > 0 {
                tracing::info!("audit cleanup: removed {time_deleted} expired records");
            }

            // 2. Size cleanup: if DB file exceeds max_size_mb, delete oldest 10%
            let db_path = conn.path().unwrap_or("audit.db");
            if let Ok(meta) = std::fs::metadata(db_path) {
                let file_size_mb = meta.len() / (1024 * 1024);
                if file_size_mb > max_size_mb {
                    let size_deleted = conn.execute(
                        "DELETE FROM audit_events WHERE id IN (
                            SELECT id FROM audit_events ORDER BY timestamp ASC
                            LIMIT (SELECT CAST(COUNT(*) * 0.1 AS INTEGER) FROM audit_events)
                        )",
                        [],
                    )?;
                    if size_deleted > 0 {
                        tracing::info!(
                            "audit cleanup: removed {size_deleted} records (size threshold: {}MB > {}MB)",
                            file_size_mb, max_size_mb
                        );
                    }
                }
            }

            // 3. Reclaim disk space
            conn.execute_batch("PRAGMA optimize;")?;

            Ok(())
        })
        .await??;

        Ok(())
    }
}
