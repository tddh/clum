#![recursion_limit = "512"]
mod audit;
mod audit_cli;
mod error;
mod files;
mod handler;
mod progress;
mod recording_sync;
mod router;
mod schema;
mod stream;
mod tools;
mod transport;
mod tunnel;

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

#[derive(Parser)]
#[command(name = "agent-ops-mcp", version, about)]
struct Cli {
    #[arg(long, default_value = "config/hosts.yaml")]
    hosts_file: PathBuf,

    #[arg(long)]
    ca_cert: String,

    #[arg(long)]
    audit_db: Option<PathBuf>,

    #[arg(long, default_value = "90")]
    audit_retention_days: u32,

    #[arg(long, default_value = "500")]
    audit_max_size_mb: u64,

    #[arg(long, default_value = "600")]
    audit_cleanup_interval_secs: u64,

    #[arg(long, default_value = "300")]
    audit_sync_interval_secs: u64,

    #[arg(long)]
    recordings_dir: Option<PathBuf>,

    #[arg(long, default_value = "90")]
    recordings_retention_days: u32,

    #[arg(long, default_value = "5000")]
    recordings_max_size_mb: u64,
}

pub(crate) fn resolve_audit_db_path(custom: Option<PathBuf>) -> PathBuf {
    custom.unwrap_or_else(|| {
        let dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".agent-ops");
        std::fs::create_dir_all(&dir).ok();
        dir.join("audit.db")
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "audit" {
        return audit_cli::run_audit_command().await;
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    let router = Arc::new(
        router::HostRouter::from_file(&cli.hosts_file).context("failed to load host registry")?,
    );
    tracing::info!("loaded {} hosts", router.len());

    let db_path = resolve_audit_db_path(cli.audit_db);
    let audit_db = Arc::new(audit::AuditDb::open(&db_path)?);
    tracing::info!("audit database: {}", db_path.display());

    let cleanup_db = audit_db.clone();
    let retention_days = cli.audit_retention_days;
    let max_size_mb = cli.audit_max_size_mb;
    let interval = cli.audit_cleanup_interval_secs;
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(interval));
        loop {
            timer.tick().await;
            if let Err(e) = cleanup_db.cleanup(retention_days, max_size_mb).await {
                tracing::error!("audit cleanup failed: {e}");
            }
        }
    });

    #[cfg(unix)]
    {
        let sig_router = Arc::clone(&router);
        let sig_audit_db = Arc::clone(&audit_db);
        tokio::spawn(async move {
            let mut sig =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("SIGHUP handler not available: {e}");
                        return;
                    }
                };
            loop {
                sig.recv().await;
                match sig_router.reload() {
                    Ok(count) => {
                        tracing::info!("SIGHUP: successfully reloaded {} hosts from config", count);
                        sig_audit_db
                            .log(agent_ops_core::types::AuditEvent {
                                event_id: uuid::Uuid::new_v4(),
                                timestamp: chrono::Utc::now(),
                                agent_name: "system".to_string(),
                                host_name: String::new(),
                                session_name: String::new(),
                                pane_id: None,
                                action: agent_ops_core::types::AuditAction::ConfigReload,
                                detail: "SIGHUP received".to_string(),
                                output_summary: None,
                                success: true,
                                duration_ms: 0,
                                error_message: None,
                            })
                            .await;
                    }
                    Err(e) => {
                        tracing::error!("SIGHUP: config reload failed: {e}");
                    }
                }
            }
        });
    }

    let recordings_dir = cli
        .recordings_dir
        .clone()
        .unwrap_or_else(recording_sync::default_recordings_dir);

    // Start the background recording sync task (pulls unsynced .cast files from
    // bridges into the local recordings directory).
    let sync_config = recording_sync::RecordingSyncConfig {
        interval_secs: cli.audit_sync_interval_secs,
        recordings_dir: recordings_dir.clone(),
        retention_days: cli.recordings_retention_days,
        max_size_mb: cli.recordings_max_size_mb,
    };
    tokio::spawn(recording_sync::run_sync_loop(
        sync_config,
        Arc::clone(&router),
        cli.ca_cert.clone(),
    ));

    let ctx = Arc::new(tools::ToolContext {
        router,
        ca_cert_path: cli.ca_cert,
        audit_db,
        agent_name: std::sync::Mutex::new("unknown".to_string()),
        tunnel_manager: Arc::new(tunnel::TunnelManager::new()),
        stream_manager: Arc::new(stream::StreamManager::new()),
        recordings_dir,
    });

    let tools_definition = schema::tools_definition();
    tracing::info!("agent-ops-mcp server starting (stdio mode)");
    handler::run_mcp_stdio_loop(ctx, tools_definition).await
}
