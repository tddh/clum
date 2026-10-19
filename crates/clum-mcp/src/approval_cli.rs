use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

use crate::approvals::ApprovalStore;
use crate::resolve_audit_db_path;

/// 审批者的 OS 身份（`SUDO_USER` 优先：sudo 下 `USER`/`LOGNAME` 通常是 root，会丢掉真实发起人）。
pub(crate) fn os_identity() -> String {
    std::env::var("SUDO_USER")
        .ok()
        .filter(|v| !v.is_empty() && v != "root")
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("LOGNAME").ok())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// 默认审批者标识。
///
/// **诚实边界**：这**不是强认证**——能改环境变量的人就能改这个值。真正的授权边界是
/// 「能登录服务端主机执行本 CLI」。此处只是让伪造留痕、提高成本：`--approver` 可被覆盖，
/// 但 `os_identity()` 会**独立**写进审计 detail，两者不一致即是伪造线索。
pub(crate) fn default_approver() -> String {
    format!("human:{}", os_identity())
}

/// `clum-mcp approval <list|approve|deny>` —— 在服务端主机上以人类身份操作待批准表。
/// 不在 MCP 工具面暴露（工具冻结在 68 个），agent key 无此通道。
pub async fn run_approval_command() -> Result<()> {
    #[derive(Parser)]
    struct ApprovalCli {
        #[command(subcommand)]
        command: ApprovalCommand,
    }

    #[derive(clap::Subcommand)]
    enum ApprovalCommand {
        /// 列出待批准项
        List {
            #[arg(long)]
            db: Option<PathBuf>,
        },
        /// 批准
        Approve {
            approval_id: String,
            /// 审批者标识。默认取 OS 用户（SUDO_USER 优先）；可覆盖，但 OS 身份会独立
            /// 写入审计 detail——两者不一致即是伪造线索。
            #[arg(long)]
            approver: Option<String>,
            #[arg(long)]
            db: Option<PathBuf>,
        },
        /// 拒绝
        Deny {
            approval_id: String,
            #[arg(long)]
            approver: Option<String>,
            #[arg(long)]
            db: Option<PathBuf>,
        },
    }

    let cli = ApprovalCli::parse_from(
        std::iter::once("clum-mcp".to_string()).chain(std::env::args().skip(2)),
    );
    match cli.command {
        ApprovalCommand::List { db } => {
            let store = ApprovalStore::open(&resolve_audit_db_path(db))?;
            store.purge_expired().await?;
            let rows = store.list_pending().await?;
            if rows.is_empty() {
                println!("no pending approvals");
                return Ok(());
            }
            for r in rows {
                println!(
                    "{}\t{}\t{}\t{}\t{}\n  fingerprint: {}\n  rule: {}\n  reason: {}\n  expires: {}",
                    r.approval_id,
                    r.status_str(),
                    r.agent_name,
                    r.tool_name,
                    r.host_name,
                    r.command,
                    r.rule_id,
                    r.reason,
                    r.expires_at.to_rfc3339()
                );
            }
        }
        ApprovalCommand::Approve {
            approval_id,
            approver,
            db,
        } => {
            let approver = approver.unwrap_or_else(default_approver);
            let db_path = resolve_audit_db_path(db);
            let store = ApprovalStore::open(&db_path)?;
            let a = store.decide(&approval_id, &approver, true).await?;
            log_decision(&db_path, &approver, &a, "approved").await?;
            println!(
                "approved {}\n  rule: {}\n  agent: {}\n  scope: {}/{}\n  detail: {}",
                a.approval_id, a.rule_id, a.agent_name, a.tool_name, a.host_name, a.reason
            );
        }
        ApprovalCommand::Deny {
            approval_id,
            approver,
            db,
        } => {
            let approver = approver.unwrap_or_else(default_approver);
            let db_path = resolve_audit_db_path(db);
            let store = ApprovalStore::open(&db_path)?;
            let a = store.decide(&approval_id, &approver, false).await?;
            log_decision(&db_path, &approver, &a, "denied").await?;
            println!(
                "denied {}\n  rule: {}\n  agent: {}\n  scope: {}/{}\n  detail: {}",
                a.approval_id, a.rule_id, a.agent_name, a.tool_name, a.host_name, a.reason
            );
        }
    }
    Ok(())
}

/// 记录人工审批决定。批准权握在人类身份手里，必须留痕（与 `audit_cli::log_cli_audit`
/// 同款做法：直接打开同一个 audit.db 写事件，保持哈希链连续）。
async fn log_decision(
    db_path: &std::path::Path,
    approver: &str,
    a: &crate::approvals::Approval,
    verb: &str,
) -> Result<()> {
    let audit_db = crate::audit::AuditDb::open(db_path)?;
    audit_db
        .log(clum_core::types::AuditEvent {
            event_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            agent_name: approver.to_string(),
            host_name: a.host_name.clone(),
            session_name: String::new(),
            pane_id: None,
            operation_id: None,
            action: clum_core::types::AuditAction::ApprovalDecided,
            detail: format!(
                "{verb} approval={} rule={} agent={} approver_os={} scope={}/{} reason={}",
                a.approval_id,
                a.rule_id,
                a.agent_name,
                // 独立于 --approver 记录：即使调用方伪造了 approver，这里仍留下环境身份
                os_identity(),
                a.tool_name,
                a.host_name,
                // a.command 现在是指纹（sha256:…），人类可读的脱敏摘要在 reason 里
                a.reason
            ),
            redacted: false,
            output_summary: None,
            success: true,
            duration_ms: 0,
            error_message: None,
        })
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F-8：审批归因不得是固定占位串。此前 `--approver` 默认写死 `human:cli`，
    /// 审计里所有批准都归到同一个假身份，事后无法追溯是谁批的。
    #[test]
    fn default_approver_is_not_a_fixed_placeholder() {
        let a = default_approver();
        assert!(a.starts_with("human:"), "实际 {a:?}");
        assert_ne!(a, "human:cli", "不得再用固定占位身份");
    }

    /// OS 身份必须能独立于 `--approver` 取得：即使调用方伪造 `--approver`，
    /// 审计里仍留有一条来自环境的身份，两者不一致即是伪造线索。
    #[test]
    fn os_identity_is_available_and_matches_default_approver() {
        let os = os_identity();
        assert!(!os.is_empty(), "OS 身份不得为空");
        assert_eq!(default_approver(), format!("human:{os}"));
    }
}
