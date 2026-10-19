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
    /// 已被消费（命令已放行执行）。一次性授权的终态，不可再次消费。
    Consumed,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Denied => "denied",
            ApprovalStatus::Expired => "expired",
            ApprovalStatus::Consumed => "consumed",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "approved" => ApprovalStatus::Approved,
            "denied" => ApprovalStatus::Denied,
            "expired" => ApprovalStatus::Expired,
            "consumed" => ApprovalStatus::Consumed,
            _ => ApprovalStatus::Pending,
        }
    }
}

/// 放行消费的结果。除 `Consumed` 外一律拒绝执行（fail-closed）——
/// 任何校验不通过都不得降级为放行。
#[derive(Debug)]
pub enum ConsumeOutcome {
    /// 校验全部通过并已原子置为 `consumed`，命令可以执行。
    /// `Box` 是 clippy `large_enum_variant` 要求：`Approval` 有 256 字节，
    /// 而其余变体最大 1 字节，不装箱会让整个 enum 膨胀到 256 字节。
    Consumed(Box<Approval>),
    /// approval_id 不存在
    NotFound,
    /// 已被裁决过（denied/expired/consumed）或仍是 pending（尚未批准）
    AlreadyDecided(ApprovalStatus),
    /// 指纹不匹配：批准后命令被改动（防"批 A 执行 B"偷渡）
    FingerprintMismatch,
    /// 申请方与当前调用方不是同一 agent（防跨 agent 盗用批准）
    WrongAgent,
    /// 工具或主机与批准时不一致（批准绑定「单命令 + 单主机 + 单工具」）
    ScopeMismatch,
    /// 已过期
    Expired,
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
    /// 新建 pending 项的默认 TTL（秒），由 server-config `approval_ttl_secs` 决定。
    default_ttl_secs: i64,
}

impl ApprovalStore {
    /// 默认审批 TTL：15 分钟（config 可覆盖）。
    pub const DEFAULT_TTL_SECS: i64 = 900;

    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_ttl(path, Self::DEFAULT_TTL_SECS)
    }

    /// 与 `open` 相同，但允许配置 TTL。SQLite 连接统一设置 `busy_timeout`：
    /// 同一 db 文件有 audit/bridge/api_keys/approval 多个连接 + CLI 子进程并发写，
    /// 没有 busy_timeout 时写碰撞会直接报 `database is locked`。
    pub fn open_with_ttl(path: &Path, default_ttl_secs: i64) -> Result<Self> {
        let conn = Connection::open(path).context("open approval db")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)
            .context("init pending_approvals")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            // Upper bound: `now + Duration::seconds(huge)` panics on overflow.
            // 30 days is far beyond any operational need.
            default_ttl_secs: default_ttl_secs.clamp(1, 30 * 24 * 60 * 60),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            default_ttl_secs: Self::DEFAULT_TTL_SECS,
        })
    }

    pub fn default_ttl_secs(&self) -> i64 {
        self.default_ttl_secs
    }

    pub async fn create(&self, req: NewApproval) -> Result<String> {
        // 顺带清理过期项：ask 是低频路径，一次 UPDATE 的开销可接受。没有后台定时任务时
        // 这是唯一的兜底，否则过期项会长期滞留 pending（`approval list` 里混着早已无效
        // 的条目，人类分不清哪条还该批）。
        self.purge_expired().await?;
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
                // `expires_at >= ?3` 与前面的过期检查形成双保险：堵住 get 与 UPDATE 之间
                // 跨过 TTL 的 TOCTOU 窗口（否则刚好过期的项仍可能被批准）。
                "UPDATE pending_approvals SET status = ?2, decided_at = ?3, approver = ?4
                 WHERE approval_id = ?1 AND status = 'pending' AND expires_at >= ?3",
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

    /// 一次性消费已批准的审批项（放行路径）。
    ///
    /// 五重校验全部通过才放行：`status='approved'` + 同 agent + 同工具 + 同主机 + 同指纹，
    /// 且未过期。原子性由单条带完整 WHERE 的 UPDATE 保证（affected==1 才算成功），
    /// 因此并发重放同一 approval_id 只会有一个赢家。
    pub async fn consume(
        &self,
        approval_id: &str,
        agent_name: &str,
        tool_name: &str,
        host_name: &str,
        fingerprint: &str,
    ) -> Result<ConsumeOutcome> {
        let conn = Arc::clone(&self.conn);
        let id = approval_id.to_string();
        let agent = agent_name.to_string();
        let tool = tool_name.to_string();
        let host = host_name.to_string();
        let fp = fingerprint.to_string();
        let now = Utc::now();

        // 原子消费：把**全部**校验条件写进 WHERE，靠 affected 行数判定成功与否。
        // 这样并发重放同一 approval_id 只会有一个赢家（另一个 affected==0），
        // 无需额外锁；也顺带堵住「先查后改」的 TOCTOU。
        let affected = tokio::task::spawn_blocking(move || -> Result<usize> {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let n = c.execute(
                "UPDATE pending_approvals
                 SET status = 'consumed', decided_at = COALESCE(decided_at, ?2)
                 WHERE approval_id = ?1 AND status = 'approved'
                   AND agent_name = ?3 AND tool_name = ?4 AND host_name = ?5
                   AND command = ?6 AND expires_at >= ?2",
                params![id, now.to_rfc3339(), agent, tool, host, fp],
            )?;
            Ok(n)
        })
        .await??;

        if affected == 1 {
            let approval = self
                .get(approval_id)
                .await?
                .context("approval vanished after consume")?;
            return Ok(ConsumeOutcome::Consumed(Box::new(approval)));
        }

        // affected == 0：下面只用于**区分拒绝原因**（给 agent 可读的错误提示），
        // 不改变「拒绝」这一结论——任何一条不满足都不得放行。
        let Some(existing) = self.get(approval_id).await? else {
            return Ok(ConsumeOutcome::NotFound);
        };
        if existing.agent_name != agent_name {
            return Ok(ConsumeOutcome::WrongAgent);
        }
        if Utc::now() > existing.expires_at {
            return Ok(ConsumeOutcome::Expired);
        }
        if existing.status != ApprovalStatus::Approved {
            return Ok(ConsumeOutcome::AlreadyDecided(existing.status));
        }
        if existing.command != fingerprint {
            return Ok(ConsumeOutcome::FingerprintMismatch);
        }
        if existing.tool_name != tool_name || existing.host_name != host_name {
            return Ok(ConsumeOutcome::ScopeMismatch);
        }
        // 到不了这里：UPDATE 的 WHERE 与上述检查等价。唯一可能是 UPDATE 与 get 之间
        // 状态被并发改动（如另一线程刚消费完）——同样按已裁决拒绝。
        Ok(ConsumeOutcome::AlreadyDecided(existing.status))
    }

    /// 查找同指纹的未过期 pending 项，供 ask 幂等复用。
    ///
    /// 没有它，agent 每次重试都会新建一条 pending，审批表被同一命令的重复项撑满，
    /// 人类也无法分辨哪条才是当前该批的。
    pub async fn find_pending_by_fingerprint(
        &self,
        agent_name: &str,
        tool_name: &str,
        host_name: &str,
        fingerprint: &str,
    ) -> Result<Option<Approval>> {
        let conn = Arc::clone(&self.conn);
        let agent = agent_name.to_string();
        let tool = tool_name.to_string();
        let host = host_name.to_string();
        let fp = fingerprint.to_string();
        let now = Utc::now().to_rfc3339();
        tokio::task::spawn_blocking(move || -> Result<Option<Approval>> {
            let c = conn.lock().unwrap_or_else(|e| e.into_inner());
            let mut stmt = c.prepare(
                "SELECT approval_id, created_at, expires_at, agent_name, caller_group, tool_name,
                        host_name, command, rule_id, reason, status, decided_at, approver
                 FROM pending_approvals
                 WHERE status = 'pending' AND agent_name = ?1 AND tool_name = ?2
                   AND host_name = ?3 AND command = ?4 AND expires_at >= ?5
                 ORDER BY created_at LIMIT 1",
            )?;
            let mut rows = stmt.query(params![agent, tool, host, fp, now])?;
            match rows.next()? {
                Some(r) => Ok(Some(row_to_approval(r)?)),
                None => Ok(None),
            }
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
            command: FP.into(),
            rule_id: "r1".into(),
            reason: "reason".into(),
            ttl_secs: 900,
        }
    }

    /// 审批表 `command` 列存的是**指纹**而非命令原文（命令原文不落盘，避免密钥明文入库），
    /// 故测试统一用指纹常量。
    const FP: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    /// 仅测试用：把 `expires_at` 改到过去，模拟「批准后、消费前跨过 TTL」。
    /// 正常 API 无法产生该状态（`decide` 会拦住已过期项），只能靠时间流逝。
    impl ApprovalStore {
        async fn backdate_expiry(&self, approval_id: &str) -> Result<()> {
            let conn = Arc::clone(&self.conn);
            let id = approval_id.to_string();
            tokio::task::spawn_blocking(move || -> Result<()> {
                let c = conn.lock().unwrap_or_else(|e| e.into_inner());
                c.execute(
                    "UPDATE pending_approvals SET expires_at = ?2 WHERE approval_id = ?1",
                    params![id, (Utc::now() - Duration::seconds(60)).to_rfc3339()],
                )?;
                Ok(())
            })
            .await?
        }
    }

    // ── consume：一次性放行 ──────────────────────────────────────

    #[tokio::test]
    async fn consume_approved_approval_succeeds_once() {
        let (s, _d) = store();
        let id = s.create(sample()).await.unwrap();
        s.decide(&id, "human:alice", true).await.unwrap();
        let decided_at = s.get(&id).await.unwrap().unwrap().decided_at;

        let out = s.consume(&id, "agent-a", "exec", "tf01", FP).await.unwrap();
        match out {
            ConsumeOutcome::Consumed(a) => {
                assert_eq!(a.approval_id, id);
                assert_eq!(
                    a.status,
                    ApprovalStatus::Consumed,
                    "消费后状态必须置为 consumed（一次性授权的终态）"
                );
                assert_eq!(
                    a.approver.as_deref(),
                    Some("human:alice"),
                    "消费不得覆盖人工批准者——审计要能追溯是谁批准的"
                );
                assert!(a.decided_at.is_some(), "消费应记录时间");
                assert_eq!(
                    a.decided_at, decided_at,
                    "消费不得覆盖人工批准时间（COALESCE 保留首次决定时刻）"
                );
            }
            other => panic!("首次消费应成功，实际 {other:?}"),
        }

        // 一次性：二次消费必须被拒（防重放同一批准执行多次）
        let again = s.consume(&id, "agent-a", "exec", "tf01", FP).await.unwrap();
        assert!(
            matches!(
                again,
                ConsumeOutcome::AlreadyDecided(ApprovalStatus::Consumed)
            ),
            "二次消费必须拒绝，实际 {again:?}"
        );
    }

    #[tokio::test]
    async fn consume_rejects_unapproved_pending() {
        // 尚未人工批准 → 不得放行（这是最关键的 fail-closed 断言）
        let (s, _d) = store();
        let id = s.create(sample()).await.unwrap();
        let out = s.consume(&id, "agent-a", "exec", "tf01", FP).await.unwrap();
        assert!(
            matches!(out, ConsumeOutcome::AlreadyDecided(ApprovalStatus::Pending)),
            "未批准的项不得放行，实际 {out:?}"
        );
    }

    #[tokio::test]
    async fn consume_rejects_denied() {
        let (s, _d) = store();
        let id = s.create(sample()).await.unwrap();
        s.decide(&id, "human:alice", false).await.unwrap();
        let out = s.consume(&id, "agent-a", "exec", "tf01", FP).await.unwrap();
        assert!(
            matches!(out, ConsumeOutcome::AlreadyDecided(ApprovalStatus::Denied)),
            "人工拒绝后不得放行，实际 {out:?}"
        );
    }

    #[tokio::test]
    async fn consume_rejects_wrong_fingerprint() {
        // 批准后改命令再重试（"批 A 执行 B" 偷渡）必须被拒
        let (s, _d) = store();
        let id = s.create(sample()).await.unwrap();
        s.decide(&id, "human:alice", true).await.unwrap();
        let out = s
            .consume(&id, "agent-a", "exec", "tf01", "sha256:other")
            .await
            .unwrap();
        assert!(
            matches!(out, ConsumeOutcome::FingerprintMismatch),
            "改命令偷渡必须被拒，实际 {out:?}"
        );
    }

    #[tokio::test]
    async fn consume_rejects_wrong_agent() {
        // 跨 agent 盗用他人的批准必须被拒
        let (s, _d) = store();
        let id = s.create(sample()).await.unwrap();
        s.decide(&id, "human:alice", true).await.unwrap();
        let out = s.consume(&id, "agent-b", "exec", "tf01", FP).await.unwrap();
        assert!(
            matches!(out, ConsumeOutcome::WrongAgent),
            "跨 agent 盗用必须被拒，实际 {out:?}"
        );
    }

    #[tokio::test]
    async fn consume_rejects_wrong_tool_or_host() {
        // 批准绑定「单命令 + 单主机 + 单工具」，换工具或换主机都不得复用
        let (s, _d) = store();
        let id = s.create(sample()).await.unwrap();
        s.decide(&id, "human:alice", true).await.unwrap();
        let out = s
            .consume(&id, "agent-a", "send_keys", "tf01", FP)
            .await
            .unwrap();
        assert!(
            matches!(out, ConsumeOutcome::ScopeMismatch),
            "换工具不得复用批准，实际 {out:?}"
        );
        let out = s
            .consume(&id, "agent-a", "exec", "other-host", FP)
            .await
            .unwrap();
        assert!(
            matches!(out, ConsumeOutcome::ScopeMismatch),
            "换主机不得复用批准，实际 {out:?}"
        );
    }

    #[tokio::test]
    async fn consume_rejects_expired_after_approval() {
        let (s, _d) = store();
        let id = s.create(sample()).await.unwrap();
        s.decide(&id, "human:alice", true).await.unwrap();
        s.backdate_expiry(&id).await.unwrap();
        let out = s.consume(&id, "agent-a", "exec", "tf01", FP).await.unwrap();
        assert!(
            matches!(out, ConsumeOutcome::Expired),
            "批准后跨过 TTL 不得放行，实际 {out:?}"
        );
    }

    #[tokio::test]
    async fn consume_rejects_unknown_id() {
        let (s, _d) = store();
        let out = s
            .consume(
                "00000000-0000-0000-0000-000000000000",
                "agent-a",
                "exec",
                "tf01",
                FP,
            )
            .await
            .unwrap();
        assert!(matches!(out, ConsumeOutcome::NotFound), "实际 {out:?}");
    }

    // ── find_pending_by_fingerprint：ask 幂等 ────────────────────

    #[tokio::test]
    async fn find_pending_matches_only_when_all_dimensions_match() {
        let (s, _d) = store();
        let id = s.create(sample()).await.unwrap();

        let hit = s
            .find_pending_by_fingerprint("agent-a", "exec", "tf01", FP)
            .await
            .unwrap();
        assert_eq!(hit.map(|a| a.approval_id), Some(id));

        // 任一维度不符都不得命中（否则批准会被跨主机/跨工具/跨 agent 复用）
        for (agent, tool, host, fp) in [
            ("agent-b", "exec", "tf01", FP),
            ("agent-a", "send_keys", "tf01", FP),
            ("agent-a", "exec", "other-host", FP),
            ("agent-a", "exec", "tf01", "sha256:other"),
        ] {
            assert!(
                s.find_pending_by_fingerprint(agent, tool, host, fp)
                    .await
                    .unwrap()
                    .is_none(),
                "维度不符却命中：agent={agent} tool={tool} host={host}"
            );
        }
    }

    #[tokio::test]
    async fn find_pending_excludes_decided_and_expired() {
        let (s, _d) = store();
        let id = s.create(sample()).await.unwrap();
        s.decide(&id, "human:alice", true).await.unwrap();
        assert!(
            s.find_pending_by_fingerprint("agent-a", "exec", "tf01", FP)
                .await
                .unwrap()
                .is_none(),
            "已批准的项不得再被当作 pending 复用"
        );

        let id2 = s.create(sample()).await.unwrap();
        s.backdate_expiry(&id2).await.unwrap();
        assert!(
            s.find_pending_by_fingerprint("agent-a", "exec", "tf01", FP)
                .await
                .unwrap()
                .is_none(),
            "已过期的 pending 不得被复用"
        );
    }

    /// F-12：没有后台清理任务时，过期项会长期滞留 `pending`（`approval list` 里
    /// 混着早已无效的条目，人类分不清哪条还该批）。create 顺带清理即可兜住。
    #[tokio::test]
    async fn create_purges_expired_pending() {
        let (s, _d) = store();
        let stale = s
            .create(NewApproval {
                ttl_secs: 0,
                ..sample()
            })
            .await
            .unwrap();
        assert_eq!(
            s.get(&stale).await.unwrap().unwrap().status,
            ApprovalStatus::Pending,
            "刚创建时应仍是 pending"
        );

        let _fresh = s.create(sample()).await.unwrap();

        assert_eq!(
            s.get(&stale).await.unwrap().unwrap().status,
            ApprovalStatus::Expired,
            "新建审批时应顺带把过期项标记为 expired"
        );
    }
}
