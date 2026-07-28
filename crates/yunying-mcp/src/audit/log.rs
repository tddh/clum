use crate::audit::AuditDb;
use yunying_core::types::AuditEvent;
use rusqlite::params;

impl AuditDb {
    /// Async wrapper: logs an audit event via spawn_blocking.
    /// Failures are silently traced — never blocks the caller.
    pub async fn log(&self, event: AuditEvent) {
        let db = self.conn_ref().clone();
        let result = tokio::task::spawn_blocking(move || {
            let conn = db.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute(
                "INSERT INTO audit_events
                    (event_id, timestamp, agent_name, host_name, session_name,
                     pane_id, action, detail, output_summary, success,
                     duration_ms, error_message)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    event.event_id.to_string(),
                    event.timestamp.to_rfc3339(),
                    event.agent_name,
                    event.host_name,
                    event.session_name,
                    event.pane_id,
                    serde_json::to_string(&event.action)
                        .unwrap_or_else(|e| {
                            tracing::error!("failed to serialize audit action: {}", e);
                            format!("{:?}", event.action)
                        })
                        .trim_matches('"')
                        .to_string(),
                    event.detail,
                    event.output_summary,
                    event.success as i32,
                    event.duration_ms as i64,
                    event.error_message,
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
