use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS pending_approvals (
        approval_id  TEXT PRIMARY KEY,
        created_at   TEXT NOT NULL,
        expires_at   TEXT NOT NULL,
        agent_name   TEXT NOT NULL,
        caller_group TEXT,
        tool_name    TEXT NOT NULL,
        host_name    TEXT NOT NULL,
        command      TEXT NOT NULL,
        rule_id      TEXT NOT NULL,
        reason       TEXT NOT NULL,
        status       TEXT NOT NULL DEFAULT 'pending',
        decided_at   TEXT,
        approver     TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_pending_approvals_status ON pending_approvals(status);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Denied => "denied",
            ApprovalStatus::Expired => "expired",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "approved" => ApprovalStatus::Approved,
            "denied" => ApprovalStatus::Denied,
            "expired" => ApprovalStatus::Expired,
            _ => ApprovalStatus::Pending,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewApproval {
    pub agent_name: String,
    pub caller_group: Option<String>,
    pub tool_name: String,
    pub host_name: String,
    pub command: String,
    pub rule_id: String,
    pub reason: String,
    pub ttl_secs: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Approval {
    pub approval_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub agent_name: String,
    pub caller_group: Option<String>,
    pub tool_name: String,
    pub host_name: String,
    pub command: String,
    pub rule_id: String,
    pub reason: String,
    pub status: ApprovalStatus,
    pub decided_at: Option<DateTime<Utc>>,
    pub approver: Option<String>,
}

impl Approval {
    pub fn status_str(&self) -> &'static str {
        self.status.as_str()
    }
}

pub struct ApprovalStore {
    conn: Arc<Mutex<Connection>>,
}

impl ApprovalStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).context("open approval db")?;
        conn.execute_batch(SCHEMA)
            .context("init pending_approvals")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn create(&self, req: NewApproval) -> Result<String> {
        let id = uuid::Uuid::now_v7().to_string();
        let id_for_insert = id.clone();
        let now = Utc::now();
        let expires = now + Duration::seconds(req.ttl_secs);
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            c.execute(
                "INSERT INTO pending_approvals
                 (approval_id, created_at, expires_at, agent_name, caller_group, tool_name,
                  host_name, command, rule_id, reason, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending')",
                params![
                    id_for_insert,
                    now.to_rfc3339(),
                    expires.to_rfc3339(),
                    req.agent_name,
                    req.caller_group,
                    req.tool_name,
                    req.host_name,
                    req.command,
                    req.rule_id,
                    req.reason
                ],
            )?;
            Ok(())
        })
        .await??;
        Ok(id)
    }

    pub async fn get(&self, approval_id: &str) -> Result<Option<Approval>> {
        let conn = Arc::clone(&self.conn);
        let id = approval_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<Approval>> {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let mut stmt = c.prepare(
                "SELECT approval_id, created_at, expires_at, agent_name, caller_group, tool_name,
                        host_name, command, rule_id, reason, status, decided_at, approver
                 FROM pending_approvals WHERE approval_id = ?1",
            )?;
            let mut rows = stmt.query(params![id])?;
            match rows.next()? {
                Some(r) => Ok(Some(row_to_approval(r)?)),
                None => Ok(None),
            }
        })
        .await?
    }

    /// 人工裁决。仅当当前为 pending 且未过期时生效；否则返回 Err。
    pub async fn decide(
        &self,
        approval_id: &str,
        approver: &str,
        approve: bool,
    ) -> Result<Approval> {
        let existing = self
            .get(approval_id)
            .await?
            .with_context(|| format!("approval '{approval_id}' not found"))?;

        if existing.status != ApprovalStatus::Pending {
            bail!("approval '{approval_id}' is already {:?}", existing.status);
        }
        if Utc::now() > existing.expires_at {
            let conn = Arc::clone(&self.conn);
            let id = approval_id.to_string();
            tokio::task::spawn_blocking(move || -> Result<()> {
                let c = conn.lock().unwrap_or_else(|e| e.into_inner());
                c.execute(
                    "UPDATE pending_approvals SET status = 'expired' WHERE approval_id = ?1",
                    params![id],
                )?;
                Ok(())
            })
            .await??;
            bail!("approval '{approval_id}' expired");
        }

        let status = if approve {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Denied
        };
        let conn = Arc::clone(&self.conn);
        let id = approval_id.to_string();
        let approver = approver.to_string();
        let now = Utc::now();
        let affected = tokio::task::spawn_blocking(move || -> Result<usize> {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let n = c.execute(
                "UPDATE pending_approvals SET status = ?2, decided_at = ?3, approver = ?4
                 WHERE approval_id = ?1 AND status = 'pending'",
                params![id, status.as_str(), now.to_rfc3339(), approver],
            )?;
            Ok(n)
        })
        .await??;

        if affected == 0 {
            bail!("approval '{approval_id}' was decided concurrently");
        }
        self.get(approval_id)
            .await?
            .context("approval vanished after update")
    }

    pub async fn list_pending(&self) -> Result<Vec<Approval>> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || -> Result<Vec<Approval>> {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let mut stmt = c.prepare(
                "SELECT approval_id, created_at, expires_at, agent_name, caller_group, tool_name,
                        host_name, command, rule_id, reason, status, decided_at, approver
                 FROM pending_approvals WHERE status = 'pending' ORDER BY created_at",
            )?;
            let rows = stmt.query_map([], |r| Ok(row_to_approval(r)))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r??);
            }
            Ok(out)
        })
        .await?
    }

    /// 把已过期的 pending 标记为 expired，返回处理条数。
    pub async fn purge_expired(&self) -> Result<usize> {
        let conn = Arc::clone(&self.conn);
        let now = Utc::now().to_rfc3339();
        tokio::task::spawn_blocking(move || -> Result<usize> {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let n = c.execute(
                "UPDATE pending_approvals SET status = 'expired'
                 WHERE status = 'pending' AND expires_at < ?1",
                params![now],
            )?;
            Ok(n)
        })
        .await?
    }
}

fn row_to_approval(r: &rusqlite::Row<'_>) -> Result<Approval> {
    let created: String = r.get(1)?;
    let expires: String = r.get(2)?;
    let status: String = r.get(10)?;
    let decided: Option<String> = r.get(11)?;
    Ok(Approval {
        approval_id: r.get(0)?,
        created_at: DateTime::parse_from_rfc3339(&created)?.with_timezone(&Utc),
        expires_at: DateTime::parse_from_rfc3339(&expires)?.with_timezone(&Utc),
        agent_name: r.get(3)?,
        caller_group: r.get(4)?,
        tool_name: r.get(5)?,
        host_name: r.get(6)?,
        command: r.get(7)?,
        rule_id: r.get(8)?,
        reason: r.get(9)?,
        status: ApprovalStatus::parse(&status),
        decided_at: decided
            .map(|d| DateTime::parse_from_rfc3339(&d).map(|t| t.with_timezone(&Utc)))
            .transpose()?,
        approver: r.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (ApprovalStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = ApprovalStore::open(&dir.path().join("audit.db")).unwrap();
        (s, dir)
    }

    #[tokio::test]
    async fn create_then_get_roundtrip() {
        let (s, _d) = store();
        let id = s
            .create(NewApproval {
                agent_name: "agent-a".into(),
                caller_group: Some("dev".into()),
                tool_name: "exec".into(),
                host_name: "tf01".into(),
                command: "systemctl restart nginx".into(),
                rule_id: "prod-write-approval".into(),
                reason: "prod 变更".into(),
                ttl_secs: 900,
            })
            .await
            .unwrap();
        let got = s.get(&id).await.unwrap().unwrap();
        assert_eq!(got.status, ApprovalStatus::Pending);
        assert_eq!(got.command, "systemctl restart nginx");
    }

    #[tokio::test]
    async fn decide_approve_changes_status_once() {
        let (s, _d) = store();
        let id = s.create(sample()).await.unwrap();
        let a = s.decide(&id, "human:alice", true).await.unwrap();
        assert_eq!(a.status, ApprovalStatus::Approved);
        assert_eq!(a.approver.as_deref(), Some("human:alice"));
        // 二次裁决必须失败（幂等保护，防重复放行）
        assert!(s.decide(&id, "human:bob", true).await.is_err());
    }

    #[tokio::test]
    async fn expired_cannot_be_approved() {
        let (s, _d) = store();
        let id = s
            .create(NewApproval {
                ttl_secs: 0,
                ..sample()
            })
            .await
            .unwrap();
        assert!(s.decide(&id, "human:alice", true).await.is_err());
        assert_eq!(
            s.get(&id).await.unwrap().unwrap().status,
            ApprovalStatus::Expired
        );
    }

    #[tokio::test]
    async fn purge_marks_expired() {
        let (s, _d) = store();
        let _ = s
            .create(NewApproval {
                ttl_secs: 0,
                ..sample()
            })
            .await
            .unwrap();
        assert_eq!(s.purge_expired().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn list_pending_only_returns_pending() {
        let (s, _d) = store();
        let a = s.create(sample()).await.unwrap();
        let b = s.create(sample()).await.unwrap();
        s.decide(&a, "human:x", false).await.unwrap();
        let pend = s.list_pending().await.unwrap();
        assert_eq!(pend.len(), 1);
        assert_eq!(pend[0].approval_id, b);
    }

    fn sample() -> NewApproval {
        NewApproval {
            agent_name: "agent-a".into(),
            caller_group: None,
            tool_name: "exec".into(),
            host_name: "tf01".into(),
            command: "rm -rf /data".into(),
            rule_id: "r1".into(),
            reason: "reason".into(),
            ttl_secs: 900,
        }
    }
}
