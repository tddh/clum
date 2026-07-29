#![recursion_limit = "512"]
mod api_keys;
mod audit;
mod audit_cli;
mod bridge_store;
mod error;
mod files;
mod handler;
mod http_server;
mod progress;
mod quic_server;
mod recording_sync;
mod registry;
mod router;
mod schema;
mod server_config;
mod stream;
mod token_rotation;
mod tools;
mod transport;
mod tunnel;

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

#[derive(Parser)]
#[command(name = "yunying-mcp", version, about)]
struct Cli {
    #[arg(long, default_value = "stdio")]
    mode: String,

    #[arg(long, default_value = "0.0.0.0:9778")]
    listen: String,

    #[arg(long, value_delimiter = ',')]
    api_keys: Vec<String>,

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

    /// TLS certificate for QUIC server (PEM). Required for http mode.
    #[arg(long)]
    server_cert: Option<String>,

    /// TLS private key for QUIC server (PEM). Required for http mode.
    #[arg(long)]
    server_key: Option<String>,

    /// Bridge token mapping: hostname=token (repeatable).
    #[arg(long = "bridge", value_name = "HOSTNAME=TOKEN")]
    bridge_tokens: Vec<String>,

    /// Path to server-config.yaml. Values are overridden by explicit CLI args.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Directory for static file serving (install.sh, releases/, ca.crt).
    #[arg(long)]
    static_dir: Option<PathBuf>,
}

pub(crate) fn resolve_audit_db_path(custom: Option<PathBuf>) -> PathBuf {
    custom.unwrap_or_else(|| {
        let dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".yunying");
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
    if args.len() > 1 && args[1] == "agent" {
        return run_agent_command(&args[2..]).await;
    }
    if args.len() > 1 && args[1] == "bridge" {
        return run_bridge_command(&args[2..]).await;
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
                            .log(yunying_core::types::AuditEvent {
                                event_id: uuid::Uuid::new_v4(),
                                timestamp: chrono::Utc::now(),
                                agent_name: "system".to_string(),
                                host_name: String::new(),
                                session_name: String::new(),
                                pane_id: None,
                                action: yunying_core::types::AuditAction::ConfigReload,
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

    let bridge_registry = Arc::new(registry::BridgeRegistry::new());

    let ctx = Arc::new(tools::ToolContext {
        router,
        ca_cert_path: cli.ca_cert,
        audit_db,
        agent_name: std::sync::Mutex::new("unknown".to_string()),
        tunnel_manager: Arc::new(tunnel::TunnelManager::new()),
        stream_manager: Arc::new(stream::StreamManager::new()),
        recordings_dir,
        bridge_registry: Arc::clone(&bridge_registry),
    });

    match cli.mode.as_str() {
        "http" => {
            let file_config = match &cli.config {
                Some(path) => match server_config::ServerConfig::load(path) {
                    Ok(c) => {
                        tracing::info!("loaded server config from {}", path.display());
                        c
                    }
                    Err(e) => {
                        tracing::error!("failed to load config: {e:#}");
                        server_config::ServerConfig::default()
                    }
                },
                None => server_config::ServerConfig::default(),
            };

            let mut token_map = file_config.bridge_token_map();
            let cert = cli.server_cert.or(file_config.server_cert);
            let key = cli.server_key.or(file_config.server_key);

            tracing::info!("yunying-mcp server starting (http mode on {})", cli.listen);

            if let (Some(cert), Some(key)) = (&cert, &key) {
                for entry in &cli.bridge_tokens {
                    if let Some((hostname, token)) = entry.split_once('=') {
                        token_map.insert(hostname.to_string(), token.to_string());
                    } else {
                        tracing::warn!("ignoring malformed --bridge entry: {entry}");
                    }
                }

                use sha2::{Digest, Sha256};
                let mut hash_map: std::collections::HashMap<String, String> = token_map
                    .iter()
                    .map(|(hostname, token)| {
                        (
                            hex::encode(Sha256::digest(token.as_bytes())),
                            hostname.clone(),
                        )
                    })
                    .collect();

                let bridge_db = bridge_store::BridgeStore::open(&db_path)?;
                let db_hashes = bridge_db.token_map().await;
                hash_map.extend(db_hashes);

                tracing::info!("loaded {} bridge tokens", hash_map.len());

                let quic_config = quic_server::QuicServerConfig {
                    listen_addr: cli.listen.clone(),
                    cert_path: cert.clone(),
                    key_path: key.clone(),
                    bridge_token_hashes: hash_map,
                    recordings_dir: ctx.recordings_dir.clone(),
                };
                let reg = Arc::clone(&bridge_registry);
                tokio::spawn(async move {
                    if let Err(e) = quic_server::run_quic_server(quic_config, reg).await {
                        tracing::error!("QUIC server failed: {e:#}");
                    }
                });

                let bridge_db = Arc::new(bridge_db);
                let rot_reg = Arc::clone(&bridge_registry);
                let rot_db = Arc::clone(&bridge_db);
                tokio::spawn(token_rotation::run_rotation_loop(rot_reg, rot_db, 24));
            } else {
                tracing::warn!("server cert/key not configured, QUIC listener disabled");
            }

            let key_store = api_keys::ApiKeyStore::open(&db_path)?;
            http_server::run_http_server(ctx, &cli.listen, key_store, cli.static_dir).await
        }
        _ => {
            let tools_definition = schema::tools_definition();
            tracing::info!("yunying-mcp server starting (stdio mode)");
            handler::run_mcp_stdio_loop(ctx, tools_definition).await
        }
    }
}

async fn run_agent_command(args: &[String]) -> anyhow::Result<()> {
    let db_path = resolve_audit_db_path(None);
    let store = api_keys::ApiKeyStore::open(&db_path)?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    match args.first().map(|s| s.as_str()) {
        Some("add") => {
            let name = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: yunying-mcp agent add <name>"))?;
            let key = store.add(name).await?;
            println!("API Key: {key}");
            println!("Please save this key. It will not be shown again.");
        }
        Some("list") => {
            let keys = store.list().await;
            println!("NAME         KEY PREFIX           CREATED                LAST USED              STATUS");
            for k in keys {
                let status = if k.revoked { "revoked" } else { "active" };
                println!(
                    "{:<12} {:<20} {:<22} {:<22} {}",
                    k.name,
                    k.key_prefix,
                    &k.created_at[..k.created_at.len().min(19)],
                    k.last_used_at
                        .as_deref()
                        .map(|s| &s[..s.len().min(19)])
                        .unwrap_or("-"),
                    status,
                );
            }
        }
        Some("rotate") => {
            let name = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: yunying-mcp agent rotate <name>"))?;
            let key = store.rotate(name).await?;
            println!("New API Key: {key}");
            println!("Old key expires in 24h.");
        }
        Some("revoke") => {
            let name = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: yunying-mcp agent revoke <name>"))?;
            store.revoke(name).await?;
            println!("Agent '{name}' revoked.");
        }
        _ => {
            eprintln!("usage: yunying-mcp agent <add|list|rotate|revoke> [name]");
            std::process::exit(1);
        }
    }
    Ok(())
}

async fn run_bridge_command(args: &[String]) -> anyhow::Result<()> {
    let db_path = resolve_audit_db_path(None);
    let store = bridge_store::BridgeStore::open(&db_path)?;

    match args.first().map(|s| s.as_str()) {
        Some("add") => {
            let hostname = args.get(1).ok_or_else(|| {
                anyhow::anyhow!("usage: yunying-mcp bridge add <hostname> [--tags gpu,web]")
            })?;
            let tags: Vec<String> = args
                .iter()
                .position(|a| a == "--tags")
                .and_then(|i| args.get(i + 1))
                .map(|t| t.split(',').map(String::from).collect())
                .unwrap_or_default();
            let token = bridge_store::generate_bridge_token();
            store.add(hostname, &token, &tags).await?;
            println!("Bridge: {hostname}");
            println!("Token:  {token}");
            println!();
            println!("Deploy command (on target machine):");
            println!("  rmux-bridge --server-addr <SERVER>:9778 --auth-token {token} --ca-cert /etc/yunying/ca.crt");
        }
        Some("list") => {
            let bridges = store.list().await;
            println!("HOSTNAME     TOKEN PREFIX  TAGS             STATUS");
            for b in bridges {
                let status = if b.revoked { "revoked" } else { "active" };
                println!(
                    "{:<12} {:<13} {:<16} {}",
                    b.hostname,
                    b.token_prefix,
                    b.tags.join(","),
                    status,
                );
            }
        }
        Some("remove") => {
            let hostname = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: yunying-mcp bridge remove <hostname>"))?;
            store.remove(hostname).await?;
            println!("Bridge '{hostname}' revoked.");
        }
        Some("join") => {
            let hostname = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: yunying-mcp bridge join <hostname>"))?;
            let token = store.join(hostname).await?;
            println!("New join token for '{hostname}': {token}");
            println!("Update the bridge's token file or env, then restart it.");
        }
        _ => {
            eprintln!("usage: yunying-mcp bridge <add|list|remove|join> [hostname]");
            std::process::exit(1);
        }
    }
    Ok(())
}
