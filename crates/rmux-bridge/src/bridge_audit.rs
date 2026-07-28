use std::path::Path;
use std::sync::Arc;

use anyhow::Context;

#[derive(Debug, Clone, serde::Serialize)]
pub struct BridgeEvent {
    pub event_type: String,
    pub client_addr: String,
    pub client_id: Option<String>,
    pub session_name: Option<String>,
    pub pane_id: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub detail: Option<serde_json::Value>,
    pub duration_secs: Option<f64>,
    pub exit_code: Option<i32>,
}

pub struct BridgeAuditDb {
    conn: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
}

impl BridgeAuditDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory: {}", parent.display()))?;
        }
        let conn = rusqlite::Connection::open(path)
            .with_context(|| format!("failed to open database: {}", path.display()))?;
        Self::init_conn(conn)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        Self::init_conn(conn)
    }

    fn init_conn(conn: rusqlite::Connection) -> anyhow::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS connect_events (
                event_id      TEXT PRIMARY KEY,
                timestamp     TEXT NOT NULL,
                client_addr   TEXT NOT NULL,
                client_id     TEXT,
                auth_method   TEXT NOT NULL DEFAULT 'token',
                event_type    TEXT NOT NULL,
                session_name  TEXT,
                pane_id       TEXT,
                cols          INTEGER,
                rows          INTEGER,
                detail        TEXT,
                duration_secs REAL,
                exit_code     INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_events_ts ON connect_events(timestamp);
            CREATE INDEX IF NOT EXISTS idx_events_type ON connect_events(event_type, timestamp);
            CREATE INDEX IF NOT EXISTS idx_events_session ON connect_events(session_name, timestamp);",
        )?;
        Ok(Self {
            conn: Arc::new(tokio::sync::Mutex::new(conn)),
        })
    }

    pub async fn log(&self, event: BridgeEvent) {
        let conn = self.conn.clone();
        let result = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let event_id = uuid::Uuid::now_v7().to_string();
            let timestamp = chrono::Utc::now().to_rfc3339();
            let detail_str = event
                .detail
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            conn.execute(
                "INSERT INTO connect_events (event_id, timestamp, client_addr, client_id, auth_method, event_type, session_name, pane_id, cols, rows, detail, duration_secs, exit_code)
                 VALUES (?1, ?2, ?3, ?4, 'token', ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    event_id,
                    timestamp,
                    event.client_addr,
                    event.client_id,
                    event.event_type,
                    event.session_name,
                    event.pane_id,
                    event.cols,
                    event.rows,
                    detail_str,
                    event.duration_secs,
                    event.exit_code,
                ],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await;

        if let Err(e) = result {
            tracing::error!("bridge audit log task panicked: {e}");
        } else if let Err(e) = result.unwrap() {
            tracing::error!("bridge audit log failed: {e}");
        }
    }

    pub async fn query(
        &self,
        event_type: Option<&str>,
        session_name: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let event_type = event_type.map(|s| s.to_string());
        let session_name = session_name.map(|s| s.to_string());
        let since = since.map(|s| s.to_string());
        let until = until.map(|s| s.to_string());
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut sql = String::from("SELECT event_id, timestamp, client_addr, client_id, auth_method, event_type, session_name, pane_id, cols, rows, detail, duration_secs, exit_code FROM connect_events");
            let mut conditions: Vec<String> = Vec::new();
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

            if let Some(ref et) = event_type {
                conditions.push("event_type = ?".to_string());
                params.push(Box::new(et.clone()));
            }
            if let Some(ref sn) = session_name {
                conditions.push("session_name = ?".to_string());
                params.push(Box::new(sn.clone()));
            }
            if let Some(ref s) = since {
                conditions.push("timestamp >= ?".to_string());
                params.push(Box::new(s.clone()));
            }
            if let Some(ref u) = until {
                conditions.push("timestamp <= ?".to_string());
                params.push(Box::new(u.clone()));
            }

            if !conditions.is_empty() {
                sql.push_str(" WHERE ");
                sql.push_str(&conditions.join(" AND "));
            }
            sql.push_str(" ORDER BY timestamp DESC LIMIT ?");
            params.push(Box::new(limit));

            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();

            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(param_refs.as_slice(), |row| {
                let detail: Option<String> = row.get(10)?;
                let detail_value: Option<serde_json::Value> = detail
                    .and_then(|d| serde_json::from_str(&d).ok());
                Ok(serde_json::json!({
                    "event_id": row.get::<_, String>(0)?,
                    "timestamp": row.get::<_, String>(1)?,
                    "client_addr": row.get::<_, String>(2)?,
                    "client_id": row.get::<_, Option<String>>(3)?,
                    "auth_method": row.get::<_, String>(4)?,
                    "event_type": row.get::<_, String>(5)?,
                    "session_name": row.get::<_, Option<String>>(6)?,
                    "pane_id": row.get::<_, Option<String>>(7)?,
                    "cols": row.get::<_, Option<i64>>(8)?,
                    "rows": row.get::<_, Option<i64>>(9)?,
                    "detail": detail_value,
                    "duration_secs": row.get::<_, Option<f64>>(11)?,
                    "exit_code": row.get::<_, Option<i64>>(12)?,
                }))
            })?;

            let mut results = Vec::new();
            for row in rows {
                results.push(row?);
            }
            Ok(results)
        })
        .await
        .context("query task panicked")?
    }

    pub async fn stats(
        &self,
        since: Option<&str>,
    ) -> anyhow::Result<(u64, serde_json::Map<String, serde_json::Value>)> {
        let since = since.map(|s| s.to_string());
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            let (total, by_type) = if let Some(ref s) = since {
                let total: u64 = conn.query_row(
                    "SELECT COUNT(*) FROM connect_events WHERE timestamp >= ?1",
                    rusqlite::params![s],
                    |row| row.get(0),
                )?;
                let mut stmt = conn.prepare(
                    "SELECT event_type, COUNT(*) FROM connect_events WHERE timestamp >= ?1 GROUP BY event_type",
                )?;
                let rows = stmt.query_map(rusqlite::params![s], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                })?;
                let mut map = serde_json::Map::new();
                for row in rows {
                    let (et, count) = row?;
                    map.insert(et, serde_json::Value::from(count));
                }
                (total, map)
            } else {
                let total: u64 = conn.query_row(
                    "SELECT COUNT(*) FROM connect_events",
                    [],
                    |row| row.get(0),
                )?;
                let mut stmt = conn.prepare(
                    "SELECT event_type, COUNT(*) FROM connect_events GROUP BY event_type",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                })?;
                let mut map = serde_json::Map::new();
                for row in rows {
                    let (et, count) = row?;
                    map.insert(et, serde_json::Value::from(count));
                }
                (total, map)
            };

            Ok((total, by_type))
        })
        .await
        .context("stats task panicked")?
    }

    pub async fn cleanup(&self, retention_days: u32, _max_size_mb: u64) -> anyhow::Result<usize> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(retention_days));
            let cutoff_str = cutoff.to_rfc3339();
            let deleted = conn.execute(
                "DELETE FROM connect_events WHERE timestamp < ?1",
                rusqlite::params![cutoff_str],
            )?;
            conn.pragma_update(None, "optimize", "")?;
            Ok(deleted)
        })
        .await
        .context("cleanup task panicked")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_log_and_query() {
        let db = BridgeAuditDb::open_in_memory().unwrap();

        db.log(BridgeEvent {
            event_type: "auth_success".to_string(),
            client_addr: "127.0.0.1:12345".to_string(),
            client_id: Some("client-1".to_string()),
            session_name: Some("yunying".to_string()),
            pane_id: Some("%0".to_string()),
            cols: Some(80),
            rows: Some(24),
            detail: Some(serde_json::json!({"method": "token"})),
            duration_secs: Some(1.5),
            exit_code: None,
        })
        .await;

        db.log(BridgeEvent {
            event_type: "auth_failure".to_string(),
            client_addr: "192.168.1.100:54321".to_string(),
            client_id: None,
            session_name: None,
            pane_id: None,
            cols: None,
            rows: None,
            detail: None,
            duration_secs: None,
            exit_code: Some(1),
        })
        .await;

        // Query all
        let all = db.query(None, None, None, None, 100).await.unwrap();
        assert_eq!(all.len(), 2);

        // Query by event_type
        let failures = db
            .query(Some("auth_failure"), None, None, None, 100)
            .await
            .unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["event_type"], "auth_failure");
        assert_eq!(failures[0]["client_addr"], "192.168.1.100:54321");

        // Query by session_name
        let sessions = db
            .query(None, Some("yunying"), None, None, 100)
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_name"], "yunying");
        assert_eq!(sessions[0]["pane_id"], "%0");
        assert_eq!(sessions[0]["detail"]["method"], "token");
    }

    #[tokio::test]
    async fn test_cleanup() {
        let db = BridgeAuditDb::open_in_memory().unwrap();

        // Insert an old event directly via SQL
        {
            let conn = db.conn.clone();
            tokio::task::spawn_blocking(move || {
                let conn = conn.blocking_lock();
                conn.execute(
                    "INSERT INTO connect_events (event_id, timestamp, client_addr, client_id, auth_method, event_type, session_name, pane_id, cols, rows, detail, duration_secs, exit_code)
                     VALUES ('old-event', '2020-01-01T00:00:00+00:00', '10.0.0.1:1111', NULL, 'token', 'auth_success', NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
                    [],
                )
                .unwrap();
            })
            .await
            .unwrap();
        }

        // Log a new event
        db.log(BridgeEvent {
            event_type: "auth_success".to_string(),
            client_addr: "127.0.0.1:9999".to_string(),
            client_id: None,
            session_name: Some("test-session".to_string()),
            pane_id: None,
            cols: None,
            rows: None,
            detail: None,
            duration_secs: None,
            exit_code: None,
        })
        .await;

        // Cleanup with 90 days retention
        let deleted = db.cleanup(90, 50).await.unwrap();
        assert_eq!(deleted, 1);

        // Query all -> 1 remaining
        let remaining = db.query(None, None, None, None, 100).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0]["client_addr"], "127.0.0.1:9999");
    }
}
