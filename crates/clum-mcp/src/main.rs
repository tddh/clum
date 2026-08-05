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
#[command(name = "clum-mcp", version, about)]
struct Cli {
    #[arg(long, default_value = "stdio")]
    mode: String,

    #[arg(long)]
    listen: Option<String>,

    #[arg(long, value_delimiter = ',')]
    api_keys: Vec<String>,

    #[arg(long)]
    hosts_file: Option<PathBuf>,

    #[arg(long)]
    ca_cert: Option<String>,

    #[arg(long)]
    audit_db: Option<PathBuf>,

    #[arg(long)]
    audit_retention_days: Option<u32>,

    #[arg(long)]
    audit_max_size_mb: Option<u64>,

    #[arg(long)]
    audit_cleanup_interval_secs: Option<u64>,

    #[arg(long)]
    audit_sync_interval_secs: Option<u64>,

    #[arg(long)]
    recordings_dir: Option<PathBuf>,

    #[arg(long)]
    recordings_retention_days: Option<u32>,

    #[arg(long)]
    recordings_max_size_mb: Option<u64>,

    #[arg(long)]
    server_cert: Option<String>,

    #[arg(long)]
    server_key: Option<String>,

    #[arg(long = "bridge", value_name = "HOSTNAME=TOKEN")]
    bridge_tokens: Vec<String>,

    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long)]
    static_dir: Option<PathBuf>,
}

pub(crate) fn resolve_audit_db_path(custom: Option<PathBuf>) -> PathBuf {
    custom.unwrap_or_else(|| {
        let dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".clum");
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

    // Merge: CLI > config file > defaults
    // Resolve methods and token map first (they borrow file_config)
    let file_audit_db = file_config.resolve_audit_db();
    let file_recordings_dir = file_config.resolve_recordings_dir();
    let file_static_dir = file_config.resolve_static_dir();
    let file_bridge_tokens = file_config.bridge_token_map();
    let token_ttl_hours = file_config.token_ttl_hours;

    let listen = cli.listen.or(Some(file_config.listen));
    let hosts_file = cli
        .hosts_file
        .or_else(|| file_config.hosts_file.as_ref().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("config/hosts.yaml"));
    let ca_cert = cli
        .ca_cert
        .or(file_config.ca_cert)
        .unwrap_or_else(|| String::from(""));
    let audit_retention_days = cli
        .audit_retention_days
        .unwrap_or(file_config.audit_retention_days);
    let audit_max_size_mb = cli
        .audit_max_size_mb
        .unwrap_or(file_config.audit_max_size_mb);
    let cleanup_interval = cli
        .audit_cleanup_interval_secs
        .unwrap_or(file_config.audit_cleanup_interval_secs);
    let sync_interval = cli
        .audit_sync_interval_secs
        .unwrap_or(file_config.audit_sync_interval_secs);
    let recordings_retention = cli
        .recordings_retention_days
        .unwrap_or(file_config.recordings_retention_days);
    let recordings_max_size = cli
        .recordings_max_size_mb
        .unwrap_or(file_config.recordings_max_size_mb);
    let static_dir = cli.static_dir.or(file_static_dir);
    let server_cert = cli.server_cert.or(file_config.server_cert);
    let server_key = cli.server_key.or(file_config.server_key);

    let router = Arc::new(
        router::HostRouter::from_file(&hosts_file).context("failed to load host registry")?,
    );
    tracing::info!("loaded {} hosts", router.len());

    let db_path = cli.audit_db.unwrap_or(file_audit_db);
    let audit_db = Arc::new(audit::AuditDb::open(&db_path)?);
    tracing::info!("audit database: {}", db_path.display());

    let cleanup_db = audit_db.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(cleanup_interval));
        loop {
            timer.tick().await;
            if let Err(e) = cleanup_db
                .cleanup(audit_retention_days, audit_max_size_mb)
                .await
            {
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
                            .log(clum_core::types::AuditEvent {
                                event_id: uuid::Uuid::new_v4(),
                                timestamp: chrono::Utc::now(),
                                agent_name: "system".to_string(),
                                host_name: String::new(),
                                session_name: String::new(),
                                pane_id: None,
                                action: clum_core::types::AuditAction::ConfigReload,
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

    let bridge_registry = Arc::new(registry::BridgeRegistry::new());

    let recordings_dir = cli
        .recordings_dir
        .or(file_recordings_dir)
        .unwrap_or_else(recording_sync::default_recordings_dir);

    let sync_config = recording_sync::RecordingSyncConfig {
        interval_secs: sync_interval,
        recordings_dir: recordings_dir.clone(),
        retention_days: recordings_retention,
        max_size_mb: recordings_max_size,
    };
    tokio::spawn(recording_sync::run_sync_loop(
        sync_config,
        Arc::clone(&router),
        Arc::clone(&bridge_registry),
        ca_cert.clone(),
    ));
    let bridge_store = Arc::new(bridge_store::BridgeStore::open(&db_path)?);

    let ctx = Arc::new(tools::ToolContext {
        router,
        ca_cert_path: ca_cert.clone(),
        audit_db: audit_db.clone(),
        agent_name: Arc::new(std::sync::Mutex::new("unknown".to_string())),
        caller_group: Arc::new(std::sync::Mutex::new(None)),
        tunnel_manager: Arc::new(tunnel::TunnelManager::new()),
        stream_manager: Arc::new(stream::StreamManager::new()),
        recordings_dir,
        bridge_registry: Arc::clone(&bridge_registry),
        bridge_store: Arc::clone(&bridge_store),
    });

    match cli.mode.as_str() {
        "http" => {
            let mut token_map = file_bridge_tokens;
            let cert = server_cert;
            let key = server_key;

            tracing::info!(
                "clum-mcp server starting (http mode on {})",
                listen.as_deref().unwrap_or("0.0.0.0:9788")
            );

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

                // File/CLI tokens only — preserved across DB refreshes.
                let static_hashes = hash_map.clone();

                let db_hashes = bridge_store.token_map().await;
                hash_map.extend(db_hashes);

                tracing::info!("loaded {} bridge tokens", hash_map.len());

                let quic_config = quic_server::QuicServerConfig {
                    listen_addr: listen.clone().unwrap_or_else(|| "0.0.0.0:9788".to_string()),
                    cert_path: cert.clone(),
                    key_path: key.clone(),
                    bridge_token_hashes: hash_map,
                    static_token_hashes: static_hashes,
                    recordings_dir: ctx.recordings_dir.clone(),
                    api_key_store: Some(api_keys::ApiKeyStore::open(&db_path)?),
                    db_path: db_path.clone(),
                    router: Arc::clone(&ctx.router),
                    ca_cert_path: ctx.ca_cert_path.clone(),
                    audit_db: Arc::clone(&audit_db),
                };
                let reg = Arc::clone(&bridge_registry);
                tokio::spawn(async move {
                    if let Err(e) = quic_server::run_quic_server(quic_config, reg).await {
                        tracing::error!("QUIC server failed: {e:#}");
                    }
                });

                let rot_reg = Arc::clone(&bridge_registry);
                let rot_db = Arc::clone(&bridge_store);
                tokio::spawn(token_rotation::run_rotation_loop(rot_reg, rot_db, 24));
            } else {
                tracing::warn!("server cert/key not configured, QUIC listener disabled");
            }

            let key_store = api_keys::ApiKeyStore::open(&db_path)?;
            let listen_str = listen.as_deref().unwrap_or("0.0.0.0:9788");
            http_server::run_http_server(
                ctx,
                listen_str,
                key_store,
                Arc::clone(&bridge_store),
                static_dir,
                cert,
                key,
                token_ttl_hours,
            )
            .await
        }
        _ => {
            let tools_definition = schema::tools_definition();
            tracing::info!("clum-mcp server starting (stdio mode)");
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
            let name = args.get(1).ok_or_else(|| {
                anyhow::anyhow!("usage: clum-mcp agent add <name> (--group <group> | --admin)")
            })?;
            let is_admin = args.iter().any(|a| a == "--admin");
            let group = args
                .iter()
                .position(|a| a == "--group")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.as_str());
            if !is_admin && group.is_none() {
                anyhow::bail!(
                    "must specify --group <group> (restricted key) or --admin (superadmin key)"
                );
            }
            let group = if is_admin { None } else { group };
            let key = store.add(name, group).await?;
            println!("API Key: {key}");
            if let Some(g) = group {
                println!("Group: {g}");
            } else {
                println!("Group: (none — superadmin)");
            }
            println!("Please save this key. It will not be shown again.");
        }
        Some("list") => {
            let keys = store.list().await;
            println!("NAME         KEY PREFIX           GROUP        CREATED                LAST USED              STATUS");
            for k in keys {
                let status = if k.revoked { "revoked" } else { "active" };
                println!(
                    "{:<12} {:<20} {:<12} {:<22} {:<22} {}",
                    k.name,
                    k.key_prefix,
                    k.group.as_deref().unwrap_or("(admin)"),
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
                .ok_or_else(|| anyhow::anyhow!("usage: clum-mcp agent rotate <name>"))?;
            let key = store.rotate(name).await?;
            println!("New API Key: {key}");
            println!("Old key expires in 24h.");
        }
        Some("revoke") => {
            let name = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: clum-mcp agent revoke <name>"))?;
            store.revoke(name).await?;
            println!("Agent '{name}' revoked.");
        }
        _ => {
            eprintln!("usage: clum-mcp agent <add|list|rotate|revoke> [name]");
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
                anyhow::anyhow!(
                    "usage: clum-mcp bridge add <hostname> --tags infra,server [--group production] [--config path]"
                )
            })?;
            let tags: Vec<String> = args
                .iter()
                .position(|a| a == "--tags")
                .and_then(|i| args.get(i + 1))
                .map(|t| t.split(',').map(String::from).collect())
                .ok_or_else(|| anyhow::anyhow!("--tags is required"))?;
            let group = args
                .iter()
                .position(|a| a == "--group")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.as_str());
            let config_path = args
                .iter()
                .position(|a| a == "--config")
                .and_then(|i| args.get(i + 1))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/etc/clum/server-config.yaml"));
            let config = server_config::ServerConfig::load(&config_path)?;
            let server_addr = &config.server_addr;

            let token = bridge_store::generate_bridge_token();
            store.add(hostname, &token, &tags, group).await?;

            println!("Bridge: {hostname}");
            println!("Token:  {token}");
            println!();
            println!("Install command (on target machine):");
            println!("  curl -fsSLk -H \"Authorization: Bearer {token}\" https://{server_addr}/releases/install.sh | \\");
            println!("    BRIDGE_TOKEN={token} SERVER_ADDR={server_addr} sh");
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
                .ok_or_else(|| anyhow::anyhow!("usage: clum-mcp bridge remove <hostname>"))?;
            store.remove(hostname).await?;
            println!("Bridge '{hostname}' revoked.");
        }
        Some("join") => {
            let hostname = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: clum-mcp bridge join <hostname>"))?;
            let token = store.join(hostname).await?;
            println!("New join token for '{hostname}': {token}");
            println!("Update the bridge's token file or env, then restart it.");
        }
        _ => {
            eprintln!("usage: clum-mcp bridge <add|list|remove|join> [hostname]");
            std::process::exit(1);
        }
    }
    Ok(())
}
