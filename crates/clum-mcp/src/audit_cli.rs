use crate::audit;
use chrono::Utc;
use clap::Parser;
use clum_core::types::{AuditAction, AuditEvent};
use std::path::PathBuf;
use uuid::Uuid;

use crate::resolve_audit_db_path;

/// Log an audit event recording that the CLI itself ran an audit subcommand.
/// Failures are ignored — auditing the audit tool must never break the command.
async fn log_cli_audit(db: &audit::AuditDb, action: AuditAction, detail: String) {
    db.log(AuditEvent {
        event_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        agent_name: "cli".to_string(),
        host_name: String::new(),
        session_name: String::new(),
        pane_id: None,
        operation_id: None,
        action,
        detail,
        output_summary: None,
        success: true,
        duration_ms: 0,
        error_message: None,
    })
    .await;
}

pub async fn run_audit_command() -> anyhow::Result<()> {
    #[derive(Parser)]
    struct AuditCli {
        #[command(subcommand)]
        command: AuditCommand,
    }

    #[derive(clap::Subcommand)]
    enum AuditCommand {
        Query {
            #[arg(long)]
            db: Option<PathBuf>,
            #[arg(long)]
            host: Option<String>,
            #[arg(long)]
            action: Option<String>,
            #[arg(long)]
            agent: Option<String>,
            #[arg(long)]
            since: Option<String>,
            #[arg(long)]
            until: Option<String>,
            #[arg(long)]
            success: Option<bool>,
            #[arg(long, default_value = "50")]
            limit: u32,
            #[arg(long, default_value = "table")]
            format: String,
        },
        Stats {
            #[arg(long)]
            db: Option<PathBuf>,
            #[arg(long)]
            since: Option<String>,
        },
        Cleanup {
            #[arg(long)]
            db: Option<PathBuf>,
            #[arg(long)]
            older_than: Option<u32>,
            #[arg(long)]
            max_size: Option<u64>,
        },
    }

    let cli = AuditCli::parse_from(
        std::iter::once("clum-mcp".to_string()).chain(std::env::args().skip(2)),
    );
    match cli.command {
        AuditCommand::Query {
            db,
            host,
            action,
            agent,
            since,
            until,
            success,
            limit,
            format,
        } => {
            let db_path = resolve_audit_db_path(db);
            let audit_db = audit::AuditDb::open(&db_path)?;
            let fmt = match format.as_str() {
                "json" => audit::query::OutputFormat::Json,
                "jsonl" => audit::query::OutputFormat::Jsonl,
                _ => audit::query::OutputFormat::Table,
            };
            let params = audit::query::QueryParams {
                host,
                action,
                agent,
                since,
                until,
                success,
                limit: Some(limit),
                host_names: None,
            };
            let result = audit_db.query(params, fmt).await?;
            println!("{}", result);
            log_cli_audit(
                &audit_db,
                AuditAction::AuditQuery,
                format!("limit={}", limit),
            )
            .await;
        }
        AuditCommand::Stats { db, since } => {
            let db_path = resolve_audit_db_path(db);
            let audit_db = audit::AuditDb::open(&db_path)?;
            let since_detail = since.clone();
            let result = audit_db.stats(since).await?;
            println!("{}", result);
            log_cli_audit(
                &audit_db,
                AuditAction::AuditStats,
                format!("since={:?}", since_detail),
            )
            .await;
        }
        AuditCommand::Cleanup {
            db,
            older_than,
            max_size,
        } => {
            let db_path = resolve_audit_db_path(db);
            let audit_db = audit::AuditDb::open(&db_path)?;
            let days = older_than.unwrap_or(90);
            let size = max_size.unwrap_or(500);
            audit_db.cleanup(days, size).await?;
            println!("Cleanup completed.");
            log_cli_audit(
                &audit_db,
                AuditAction::AuditCleanup,
                format!("older_than_days={} max_size_mb={}", days, size),
            )
            .await;
        }
    }
    Ok(())
}
