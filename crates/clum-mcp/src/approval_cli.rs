use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

use crate::approvals::ApprovalStore;
use crate::resolve_audit_db_path;

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
            #[arg(long, default_value = "human:cli")]
            approver: String,
            #[arg(long)]
            db: Option<PathBuf>,
        },
        /// 拒绝
        Deny {
            approval_id: String,
            #[arg(long, default_value = "human:cli")]
            approver: String,
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
                    "{}\t{}\t{}\t{}\t{}\n  cmd: {}\n  rule: {}\n  reason: {}\n  expires: {}",
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
            let db_path = resolve_audit_db_path(db);
            let store = ApprovalStore::open(&db_path)?;
            let a = store.decide(&approval_id, &approver, true).await?;
            log_decision(&db_path, &approver, &a, "approved").await?;
            println!(
                "approved {} ({})\n  rule: {}\n  agent: {}",
                a.approval_id, a.command, a.rule_id, a.agent_name
            );
        }
        ApprovalCommand::Deny {
            approval_id,
            approver,
            db,
        } => {
            let db_path = resolve_audit_db_path(db);
            let store = ApprovalStore::open(&db_path)?;
            let a = store.decide(&approval_id, &approver, false).await?;
            log_decision(&db_path, &approver, &a, "denied").await?;
            println!(
                "denied {} ({})\n  rule: {}\n  agent: {}",
                a.approval_id, a.command, a.rule_id, a.agent_name
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
                "{verb} approval={} rule={} agent={} cmd={}",
                a.approval_id, a.rule_id, a.agent_name, a.command
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
