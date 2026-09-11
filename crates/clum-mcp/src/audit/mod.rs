//! SQLite-backed audit database for recording every MCP tool invocation.
//! Provides schema initialisation (with WAL mode), logging, querying, statistics,
//! and time/size-based cleanup. Public modules expose CLI-facing query and cleanup
//! logic.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub mod cleanup;
pub mod log;
pub mod query;

/// Thread-safe wrapper around a SQLite connection holding the audit events table.
pub struct AuditDb {
    conn: Arc<Mutex<Connection>>,
}

impl AuditDb {
    /// Opens (or creates) the SQLite audit database at `path`, enables WAL mode,
    /// and ensures the `audit_events` table and indexes exist.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).context("failed to open audit database")?;

        // Restrict the db (and the WAL/-shm siblings the journal mode creates)
        // to owner-only regardless of the ambient umask.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        if let Some(dir) = path.parent() {
            for suffix in ["-wal", "-shm"] {
                let sibling = dir.join(format!(
                    "{}{suffix}",
                    path.file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default()
                ));
                let _ = std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o600));
            }
        }

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )
        .context("failed to set WAL mode")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS audit_events (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id       TEXT NOT NULL UNIQUE,
                timestamp      TEXT NOT NULL,
                agent_name     TEXT NOT NULL DEFAULT 'unknown',
                host_name      TEXT NOT NULL,
                session_name   TEXT NOT NULL DEFAULT '',
                pane_id        TEXT,
                operation_id   TEXT,
                action         TEXT NOT NULL,
                detail         TEXT NOT NULL DEFAULT '',
                redacted       INTEGER NOT NULL DEFAULT 0,
                output_summary TEXT,
                success        INTEGER NOT NULL DEFAULT 1,
                duration_ms    INTEGER NOT NULL DEFAULT 0,
                error_message  TEXT,
                created_at     TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE INDEX IF NOT EXISTS idx_audit_timestamp
                ON audit_events(timestamp);
            CREATE INDEX IF NOT EXISTS idx_audit_host_action
                ON audit_events(host_name, action);
            CREATE INDEX IF NOT EXISTS idx_audit_agent_time
                ON audit_events(agent_name, timestamp);
            CREATE INDEX IF NOT EXISTS idx_audit_success
                ON audit_events(success);",
        )
        .context("failed to create audit schema")?;

        migrate_operation_id(&conn)?;
        migrate_redacted(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS audit_events (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id       TEXT NOT NULL UNIQUE,
                timestamp      TEXT NOT NULL,
                agent_name     TEXT NOT NULL DEFAULT 'unknown',
                host_name      TEXT NOT NULL,
                session_name   TEXT NOT NULL DEFAULT '',
                pane_id        TEXT,
                operation_id   TEXT,
                action         TEXT NOT NULL,
                detail         TEXT NOT NULL DEFAULT '',
                redacted       INTEGER NOT NULL DEFAULT 0,
                output_summary TEXT,
                success        INTEGER NOT NULL DEFAULT 1,
                duration_ms    INTEGER NOT NULL DEFAULT 0,
                error_message  TEXT,
                created_at     TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE INDEX IF NOT EXISTS idx_audit_timestamp
                ON audit_events(timestamp);
            CREATE INDEX IF NOT EXISTS idx_audit_host_action
                ON audit_events(host_name, action);
            CREATE INDEX IF NOT EXISTS idx_audit_agent_time
                ON audit_events(agent_name, timestamp);
            CREATE INDEX IF NOT EXISTS idx_audit_success
                ON audit_events(success);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn conn_ref(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }
}

/// Add `operation_id` column to legacy databases created before v0.16.
/// Idempotent: fails silently if the column already exists.
fn migrate_operation_id(conn: &Connection) -> Result<()> {
    let result = conn.execute("ALTER TABLE audit_events ADD COLUMN operation_id TEXT", []);
    match result {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("duplicate column") => Ok(()),
        Err(e) => Err(e).context("failed to migrate audit schema (operation_id)"),
    }
}

/// Add `redacted` column to legacy databases created before v0.17.
/// Idempotent: fails silently if the column already exists.
fn migrate_redacted(conn: &Connection) -> Result<()> {
    let result = conn.execute(
        "ALTER TABLE audit_events ADD COLUMN redacted INTEGER NOT NULL DEFAULT 0",
        [],
    );
    match result {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("duplicate column") => Ok(()),
        Err(e) => Err(e).context("failed to migrate audit schema (redacted)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::query::{OutputFormat, QueryParams};
    use chrono::Utc;
    use clum_core::types::{AuditAction, AuditEvent};
    use uuid::Uuid;

    fn make_event(action: AuditAction, host: &str, success: bool) -> AuditEvent {
        AuditEvent {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            agent_name: "test-agent".into(),
            host_name: host.into(),
            session_name: "test-session".into(),
            pane_id: Some("%0".into()),
            operation_id: None,
            action,
            detail: "test detail".into(),
            redacted: false,
            output_summary: None,
            success,
            duration_ms: 42,
            error_message: if success {
                None
            } else {
                Some("test error".into())
            },
        }
    }

    #[tokio::test]
    async fn test_schema_creation() {
        // open_in_memory should create tables+indexes without error
        let db = AuditDb::open_in_memory().unwrap();
        // verify we can insert
        let event = make_event(AuditAction::HostList, "tf01", true);
        db.log(event).await;
    }

    #[tokio::test]
    async fn test_log_and_query_roundtrip() {
        let db = AuditDb::open_in_memory().unwrap();
        let event = make_event(AuditAction::Exec, "tf01", true);
        db.log(event).await;

        let result = db
            .query(
                QueryParams {
                    host: Some("tf01".into()),
                    action: Some("Exec".into()),
                    agent: None,
                    since: None,
                    until: None,
                    success: Some(true),
                    limit: Some(10),
                    host_names: None,
                },
                OutputFormat::Json,
            )
            .await
            .unwrap();

        assert!(result.contains("test-agent"));
        assert!(result.contains("tf01"));
        assert!(result.contains("Exec"));
    }

    #[tokio::test]
    async fn test_query_empty() {
        let db = AuditDb::open_in_memory().unwrap();
        let result = db
            .query(
                QueryParams {
                    host: Some("nonexistent".into()),
                    action: None,
                    agent: None,
                    since: None,
                    until: None,
                    success: None,
                    limit: Some(10),
                    host_names: None,
                },
                OutputFormat::Table,
            )
            .await
            .unwrap();

        assert!(result.contains("No events found"));
    }

    #[tokio::test]
    async fn test_stats() {
        let db = AuditDb::open_in_memory().unwrap();
        for i in 0..5 {
            let event = make_event(AuditAction::Exec, "tf01", i % 2 == 0);
            db.log(event).await;
        }
        let result = db.stats(None).await.unwrap();
        assert!(result.contains("Total events:     5"));
        assert!(result.contains("Exec"));
    }

    #[tokio::test]
    async fn test_cleanup_by_time() {
        let db = AuditDb::open_in_memory().unwrap();
        {
            let conn = db.conn_ref().lock().unwrap_or_else(|e| e.into_inner());
            conn.execute(
                "INSERT INTO audit_events (event_id, timestamp, agent_name, host_name, action, detail, success)
                 VALUES ('old-1', '2020-01-01T00:00:00Z', 'test', 'tf01', 'Exec', 'test', 1)",
                [],
            ).unwrap();
        }

        db.cleanup(90, 500).await.unwrap();

        let result = db
            .query(
                QueryParams {
                    host: None,
                    action: None,
                    agent: None,
                    since: None,
                    until: None,
                    success: None,
                    limit: Some(100),
                    host_names: None,
                },
                OutputFormat::Json,
            )
            .await
            .unwrap();

        assert!(
            !result.contains("old-1"),
            "old record should have been cleaned up"
        );
    }

    #[tokio::test]
    async fn test_table_format() {
        let db = AuditDb::open_in_memory().unwrap();
        let event = make_event(AuditAction::SessionCreate, "tf02", true);
        db.log(event).await;

        let result = db
            .query(
                QueryParams {
                    host: None,
                    action: None,
                    agent: None,
                    since: None,
                    until: None,
                    success: None,
                    limit: Some(10),
                    host_names: None,
                },
                OutputFormat::Table,
            )
            .await
            .unwrap();

        assert!(result.contains("ID"));
        assert!(result.contains("TIME"));
        assert!(result.contains("HOST"));
        assert!(result.contains("AGENT"));
        assert!(result.contains("ACTION"));
        assert!(result.contains("✅"));
    }

    #[tokio::test]
    async fn test_jsonl_format() {
        let db = AuditDb::open_in_memory().unwrap();
        let event = make_event(AuditAction::FileUpload, "tf01", true);
        db.log(event).await;

        let result = db
            .query(
                QueryParams {
                    host: None,
                    action: None,
                    agent: None,
                    since: None,
                    until: None,
                    success: None,
                    limit: Some(10),
                    host_names: None,
                },
                OutputFormat::Jsonl,
            )
            .await
            .unwrap();

        assert!(result.contains("test-agent"));
        assert!(result.contains("FileUpload"));
    }

    #[tokio::test]
    async fn test_redacted_roundtrip() {
        let db = AuditDb::open_in_memory().unwrap();
        let mut event = make_event(AuditAction::SendKeys, "tf01", true);
        event.detail = "[REDACTED:8 bytes]".to_string();
        event.redacted = true;
        db.log(event).await;

        let result = db
            .query(
                QueryParams {
                    host: Some("tf01".into()),
                    action: Some("SendKeys".into()),
                    agent: None,
                    since: None,
                    until: None,
                    success: None,
                    limit: Some(10),
                    host_names: None,
                },
                OutputFormat::Json,
            )
            .await
            .unwrap();

        assert!(result.contains("[REDACTED:8 bytes]"));
        assert!(
            result.contains("\"redacted\": true"),
            "查询结果应包含 redacted 标志: {result}"
        );
    }

    #[tokio::test]
    async fn test_migrate_redacted_on_legacy_db() {
        let path = std::env::temp_dir().join(format!(
            "clum-audit-legacy-test-{}.db",
            uuid::Uuid::new_v4()
        ));
        // 构造 v0.16 形态的旧库：无 redacted 列
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE audit_events (
                    id             INTEGER PRIMARY KEY AUTOINCREMENT,
                    event_id       TEXT NOT NULL UNIQUE,
                    timestamp      TEXT NOT NULL,
                    agent_name     TEXT NOT NULL DEFAULT 'unknown',
                    host_name      TEXT NOT NULL,
                    session_name   TEXT NOT NULL DEFAULT '',
                    pane_id        TEXT,
                    operation_id   TEXT,
                    action         TEXT NOT NULL,
                    detail         TEXT NOT NULL DEFAULT '',
                    output_summary TEXT,
                    success        INTEGER NOT NULL DEFAULT 1,
                    duration_ms    INTEGER NOT NULL DEFAULT 0,
                    error_message  TEXT,
                    created_at     TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            )
            .unwrap();
        }
        // open() 应触发幂等迁移
        let db = AuditDb::open(&path).unwrap();
        let mut event = make_event(AuditAction::SendKeys, "legacy", true);
        event.redacted = true;
        db.log(event).await;

        let result = db
            .query(
                QueryParams {
                    host: Some("legacy".into()),
                    action: None,
                    agent: None,
                    since: None,
                    until: None,
                    success: None,
                    limit: None,
                    host_names: None,
                },
                OutputFormat::Json,
            )
            .await
            .unwrap();
        assert!(
            result.contains("\"redacted\": true"),
            "旧库迁移后应支持 redacted: {result}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_open_restricts_db_permissions_to_owner_only() {
        let path =
            std::env::temp_dir().join(format!("clum-audit-perm-test-{}.db", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"").unwrap();
        let _ = AuditDb::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "audit db must be owner-only after open");
        std::fs::remove_file(&path).ok();
    }
}
