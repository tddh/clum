use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use sha2::{Digest, Sha256};

use crate::registry::{BridgeConn, BridgeRegistry};

/// Relay buffer size for CLI file transfer streams through central server.
/// Must align with bridge CHUNK_SIZE and MCP COPY_BUF_SIZE (both 1 MB).
const RELAY_BUF_SIZE: usize = 1024 * 1024; // 1 MB

/// Maximum size of a single recording file accepted via Push path.
/// Rejecting oversized recordings prevents OOM from a compromised bridge.
/// Mirrors the limit enforced in Pull path (recording_sync.rs).
const MAX_PUSH_RECORDING_SIZE: u64 = 256 * 1024 * 1024; // 256 MB

pub struct QuicServerConfig {
    pub listen_addr: String,
    pub cert_path: String,
    pub key_path: String,
    /// SHA-256(token) hex → hostname
    pub bridge_token_hashes: HashMap<String, String>,
    /// Tokens supplied via config file / CLI flags only (not in the DB).
    /// Preserved across background DB refreshes.
    pub static_token_hashes: HashMap<String, String>,
    pub recordings_dir: std::path::PathBuf,
    pub api_key_store: Option<Arc<crate::api_keys::ApiKeyStore>>,
    pub db_path: std::path::PathBuf,
    pub router: Arc<crate::router::HostRouter>,
    pub ca_cert_path: String,
    pub audit_db: Arc<crate::audit::AuditDb>,
}

async fn refresh_bridge_state(
    token_map: &tokio::sync::RwLock<HashMap<String, String>>,
    host_groups: &tokio::sync::RwLock<HashMap<String, String>>,
    db_path: &std::path::Path,
    static_tokens: &HashMap<String, String>,
) {
    let Ok(store) = crate::bridge_store::BridgeStore::open(db_path) else {
        return;
    };
    let mut db_hashes = store.token_map().await;
    if !db_hashes.is_empty() || !static_tokens.is_empty() {
        // DB is authoritative for DB-managed tokens; static file/CLI tokens
        // are re-added so refresh never wipes them.
        db_hashes.extend(static_tokens.iter().map(|(k, v)| (k.clone(), v.clone())));
        let mut map = token_map.write().await;
        *map = db_hashes;
    }
    let meta = store.get_all_host_meta().await;
    let mut groups = host_groups.write().await;
    groups.clear();
    for m in meta {
        if !m.group.is_empty() {
            groups.insert(m.hostname, m.group);
        }
    }
}

pub async fn run_quic_server(
    config: QuicServerConfig,
    registry: Arc<BridgeRegistry>,
) -> anyhow::Result<()> {
    let tls_config = load_server_tls(&config.cert_path, &config.key_path)?;

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
    let transport =
        Arc::get_mut(&mut server_config.transport).context("transport Arc is shared")?;
    transport.max_concurrent_bidi_streams(256u32.into());
    transport.stream_receive_window(quinn::VarInt::from_u32(16 * 1024 * 1024));
    transport.send_window(16 * 1024 * 1024);
    transport.receive_window(quinn::VarInt::from_u32(16 * 1024 * 1024));
    transport.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(15)));
    transport.max_idle_timeout(Some(
        std::time::Duration::from_secs(120).try_into().unwrap(),
    ));

    let addr: SocketAddr = config
        .listen_addr
        .parse()
        .context("invalid QUIC listen addr")?;
    let endpoint = quinn::Endpoint::server(server_config, addr)?;
    tracing::info!("QUIC server listening on udp/{addr} (ALPN: clum)");

    let token_map: Arc<tokio::sync::RwLock<HashMap<String, String>>> =
        Arc::new(tokio::sync::RwLock::new(config.bridge_token_hashes));
    let api_key_store = config.api_key_store.clone();
    let last_agents: Arc<tokio::sync::RwLock<HashMap<String, String>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    // hostname → registered group (from bridge registration DB)
    let host_groups: Arc<tokio::sync::RwLock<HashMap<String, String>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    // Background token + group refresh from DB every 30s
    let refresh_map = Arc::clone(&token_map);
    let refresh_groups = Arc::clone(&host_groups);
    let refresh_db = config.db_path.clone();
    let refresh_static = config.static_token_hashes.clone();
    tokio::spawn(async move {
        refresh_bridge_state(&refresh_map, &refresh_groups, &refresh_db, &refresh_static).await;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            refresh_bridge_state(&refresh_map, &refresh_groups, &refresh_db, &refresh_static).await;
        }
    });

    while let Some(incoming) = endpoint.accept().await {
        let registry = Arc::clone(&registry);
        let token_map = Arc::clone(&token_map);
        let rec_dir = config.recordings_dir.clone();
        let store = api_key_store.clone();
        let agents = Arc::clone(&last_agents);
        let router = Arc::clone(&config.router);
        let ca_cert = config.ca_cert_path.clone();
        let audit = Arc::clone(&config.audit_db);
        let groups = Arc::clone(&host_groups);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(
                incoming, registry, token_map, rec_dir, store, agents, router, ca_cert, audit,
                groups,
            )
            .await
            {
                tracing::debug!("QUIC connection handler ended: {e}");
            }
        });
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    incoming: quinn::Incoming,
    registry: Arc<BridgeRegistry>,
    token_map: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    recordings_dir: std::path::PathBuf,
    api_key_store: Option<Arc<crate::api_keys::ApiKeyStore>>,
    last_agents: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    router: Arc<crate::router::HostRouter>,
    ca_cert_path: String,
    audit_db: Arc<crate::audit::AuditDb>,
    host_groups: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
) -> anyhow::Result<()> {
    let conn = incoming.await?;
    let remote_addr = conn.remote_address();

    let (mut send, mut recv) = conn.accept_bi().await?;

    let first_msg = read_frame(&mut recv).await?;
    let msg: serde_json::Value = serde_json::from_slice(&first_msg)?;

    match msg.get("type").and_then(|v| v.as_str()) {
        Some("bridge_register") => {
            handle_bridge_registration(
                conn,
                remote_addr,
                send,
                recv,
                msg,
                registry,
                token_map,
                recordings_dir,
                last_agents,
            )
            .await
        }
        Some("agent_connect") => {
            handle_agent_connection(
                conn,
                remote_addr,
                send,
                recv,
                msg,
                registry,
                api_key_store,
                last_agents,
                router,
                ca_cert_path,
                audit_db,
                host_groups,
            )
            .await
        }
        _ => {
            write_frame(
                &mut send,
                &serde_json::json!({"type": "error", "ok": false, "error": "unknown message type"}),
            )
            .await?;
            conn.close(quinn::VarInt::from_u32(0), b"protocol error");
            anyhow::bail!("unexpected first message from {remote_addr}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_bridge_registration(
    conn: quinn::Connection,
    remote_addr: std::net::SocketAddr,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    reg: serde_json::Value,
    registry: Arc<BridgeRegistry>,
    token_map: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    recordings_dir: std::path::PathBuf,
    last_agents: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
) -> anyhow::Result<()> {
    let token = reg
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let token_hash = hex::encode(Sha256::digest(token.as_bytes()));

    let hostname = match token_map.read().await.get(&token_hash) {
        Some(h) => h.clone(),
        None => {
            tracing::warn!(%remote_addr, "registration rejected: unknown token");
            write_frame(
                &mut send,
                &serde_json::json!({
                    "type": "register_ack",
                    "ok": false,
                    "error": "invalid token"
                }),
            )
            .await?;
            conn.close(quinn::VarInt::from_u32(0), b"auth failed");
            anyhow::bail!("auth failed from {remote_addr}");
        }
    };

    let version = reg
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let machine_id = reg
        .get("machine_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let os_info = reg
        .get("os_info")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let capabilities: Vec<String> = reg
        .get("capabilities")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    write_frame(
        &mut send,
        &serde_json::json!({
            "type": "register_ack",
            "ok": true,
            "hostname": hostname
        }),
    )
    .await?;

    let bridge_conn = BridgeConn {
        conn: conn.clone(),
        hostname: hostname.clone(),
        tags: Vec::new(),
        labels: HashMap::new(),
        capabilities,
        version,
        machine_id,
        os_info,
        registered_at: std::time::Instant::now(),
        last_heartbeat: tokio::sync::RwLock::new(std::time::Instant::now()),
        control_send: tokio::sync::Mutex::new(send),
    };

    if let Err(e) = registry.register(bridge_conn).await {
        conn.close(quinn::VarInt::from_u32(0), b"duplicate");
        anyhow::bail!("registration failed for {hostname}: {e}");
    }

    tracing::info!(%hostname, %remote_addr, "bridge registration complete, entering heartbeat loop");

    // Control stream reader: handle pings from Bridge
    let ctrl_hostname = hostname.clone();
    let ctrl_registry = Arc::clone(&registry);
    tokio::spawn(async move {
        while let Ok(data) = read_frame(&mut recv).await {
            let msg: serde_json::Value = match serde_json::from_slice(&data) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if msg.get("type").and_then(|v| v.as_str()) == Some("ping") {
                ctrl_registry.update_heartbeat(&ctrl_hostname).await;
            }
        }
    });

    // Accept Bridge-initiated streams (recording pushes)
    loop {
        match conn.accept_bi().await {
            Ok((push_send, push_recv)) => {
                let dir = recordings_dir.clone();
                let host = hostname.clone();
                let agents = Arc::clone(&last_agents);
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_push_stream(push_send, push_recv, &dir, &host, &agents).await
                    {
                        tracing::debug!("push stream from {host} ended: {e}");
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => break,
            Err(quinn::ConnectionError::LocallyClosed) => break,
            Err(_) => break,
        }
    }

    registry.unregister(&hostname).await;
    tracing::info!(%hostname, "bridge disconnected");
    Ok(())
}

async fn handle_push_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    recordings_dir: &std::path::Path,
    hostname: &str,
    last_agents: &tokio::sync::RwLock<HashMap<String, String>>,
) -> anyhow::Result<()> {
    let header_data = read_frame(&mut recv).await?;
    let header: serde_json::Value = serde_json::from_slice(&header_data)?;

    if header.get("type").and_then(|v| v.as_str()) != Some("recording_push") {
        anyhow::bail!("unexpected push type");
    }

    let raw_filename = header
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown.cast");
    let size = header.get("size").and_then(|v| v.as_u64()).unwrap_or(0);

    // Reject oversized recordings to prevent OOM (mirrors Pull path limit)
    if size > MAX_PUSH_RECORDING_SIZE {
        anyhow::bail!(
            "recording too large: {} bytes (max {})",
            size,
            MAX_PUSH_RECORDING_SIZE
        );
    }
    let size = size as usize;

    // Reject path traversal in filename
    if raw_filename.contains("..") || raw_filename.contains('/') || raw_filename.contains('\\') {
        anyhow::bail!("unsafe recording filename: {raw_filename}");
    }

    let agent = last_agents
        .read()
        .await
        .get(hostname)
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());

    // Reject path traversal in agent name as well
    if agent.contains("..") || agent.contains('/') || agent.contains('\\') {
        anyhow::bail!("unsafe agent name in recording push: {agent}");
    }

    let safe_filename = format!("{agent}_{raw_filename}");

    // Use date-based layout ({host}/{date}/{file}) so Push files are
    // discoverable by list_local_recordings and subject to cleanup policies.
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let host_dir = recordings_dir.join(hostname).join(&today);
    tokio::fs::create_dir_all(&host_dir).await?;
    let file_path = host_dir.join(&safe_filename);

    let mut file_data = vec![0u8; size];
    recv.read_exact(&mut file_data).await?;
    tokio::fs::write(&file_path, &file_data).await?;

    tracing::info!(%hostname, %safe_filename, %agent, size, "recording received");

    let ack = serde_json::json!({"type": "recording_ack", "ok": true});
    let ack_data = serde_json::to_vec(&ack)?;
    let len = (ack_data.len() as u32).to_le_bytes();
    send.write_all(&len).await?;
    send.write_all(&ack_data).await?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_agent_connection(
    conn: quinn::Connection,
    remote_addr: std::net::SocketAddr,
    mut send: quinn::SendStream,
    _recv: quinn::RecvStream,
    msg: serde_json::Value,
    registry: Arc<BridgeRegistry>,
    api_key_store: Option<Arc<crate::api_keys::ApiKeyStore>>,
    last_agents: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    router: Arc<crate::router::HostRouter>,
    ca_cert_path: String,
    audit_db: Arc<crate::audit::AuditDb>,
    host_groups: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
) -> anyhow::Result<()> {
    // Validate API key if auth is enabled
    let mut agent_name = "unknown".to_string();
    let mut caller_group: Option<String> = None;
    if let Some(store) = &api_key_store {
        if !store.is_empty().await {
            let key = msg.get("api_key").and_then(|v| v.as_str()).unwrap_or("");
            match store.validate(key).await {
                Some(identity) => {
                    agent_name = identity.name;
                    caller_group = identity.group;
                }
                None => {
                    write_frame(&mut send, &serde_json::json!({"type": "agent_ack", "ok": false, "error": "invalid api key"})).await?;
                    conn.close(quinn::VarInt::from_u32(0), b"auth failed");
                    anyhow::bail!("agent auth failed from {remote_addr}");
                }
            }
        }
    }

    let host = msg
        .get("host")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    if host.is_empty() {
        write_frame(
            &mut send,
            &serde_json::json!({"type": "agent_ack", "ok": false, "error": "missing host"}),
        )
        .await?;
        anyhow::bail!("agent_connect without host from {remote_addr}");
    }

    // Group isolation: reject before establishing any bridge connection.
    // The runtime registration DB is authoritative; hosts.yaml is a fallback.
    if let Some(ref cg) = caller_group {
        let registered_group = host_groups.read().await.get(&host).cloned();
        let host_group = registered_group.filter(|g| !g.is_empty()).or_else(|| {
            router
                .get(&host)
                .map(|h| h.group.clone())
                .filter(|g| !g.is_empty())
        });
        if host_group.as_deref() != Some(cg.as_str()) {
            write_frame(
                &mut send,
                &serde_json::json!({"type": "agent_ack", "ok": false, "error": format!("host '{host}' is not in your group '{cg}'")}),
            )
            .await?;
            conn.close(quinn::VarInt::from_u32(0), b"forbidden");
            anyhow::bail!(
                "agent group mismatch: caller={cg} host={host_group:?} from {remote_addr}"
            );
        }
    }

    let bridge_conn: quinn::Connection = if let Some(b) = registry.get(&host).await {
        b.conn.clone()
    } else if let Some(h) = router.get(&host) {
        let addr = h
            .bridge_addr
            .as_deref()
            .with_context(|| format!("host '{host}': bridge_addr not configured"))?;
        let token = h
            .bridge_token
            .as_deref()
            .with_context(|| format!("host '{host}': bridge_token not configured"))?;
        let (direct_conn, _auth_send, _auth_recv) =
            crate::transport::connect_to_bridge_quic(addr, token, &ca_cert_path)
                .await
                .with_context(|| format!("direct connect to {}:{} failed", host, addr))?;
        direct_conn
    } else {
        write_frame(&mut send, &serde_json::json!({"type": "agent_ack", "ok": false, "error": format!("host '{host}' not found")})).await?;
        anyhow::bail!("agent requested unknown host '{host}' from {remote_addr}");
    };

    write_frame(
        &mut send,
        &serde_json::json!({"type": "agent_ack", "ok": true, "host": host}),
    )
    .await?;
    last_agents
        .write()
        .await
        .insert(host.clone(), agent_name.clone());
    tracing::info!(%host, %remote_addr, %agent_name, "agent connected, starting relay");

    let purpose = msg
        .get("purpose")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    audit_db
        .log(clum_core::types::AuditEvent {
            event_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            agent_name: agent_name.clone(),
            host_name: host.clone(),
            session_name: String::new(),
            pane_id: None,
            action: clum_core::types::AuditAction::AgentRelay,
            detail: format!("purpose={purpose} addr={remote_addr}"),
            output_summary: None,
            success: true,
            duration_ms: 0,
            error_message: None,
        })
        .await;

    // Relay loop: for each stream CLI opens, open corresponding stream to Bridge and relay
    loop {
        match conn.accept_bi().await {
            Ok((cli_send, cli_recv)) => {
                let bc = bridge_conn.clone();
                tokio::spawn(async move {
                    if let Err(e) = relay_stream(cli_send, cli_recv, &bc).await {
                        tracing::debug!("relay stream ended: {e}");
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => break,
            Err(quinn::ConnectionError::LocallyClosed) => break,
            Err(_) => break,
        }
    }

    tracing::info!(%host, %remote_addr, "agent disconnected");
    Ok(())
}

async fn relay_stream(
    cli_send: quinn::SendStream,
    cli_recv: quinn::RecvStream,
    bridge_conn: &quinn::Connection,
) -> anyhow::Result<()> {
    let (bridge_send, bridge_recv) = bridge_conn.open_bi().await?;

    let mut cli_recv = cli_recv;
    let mut bridge_send = bridge_send;
    let mut bridge_recv = bridge_recv;
    let mut cli_send = cli_send;

    let c2b = async {
        let mut buf = vec![0u8; RELAY_BUF_SIZE];
        while let Ok(Some(n)) = cli_recv.read(&mut buf).await {
            if bridge_send.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = bridge_send.finish();
    };

    let b2c = async {
        let mut buf = vec![0u8; RELAY_BUF_SIZE];
        while let Ok(Some(n)) = bridge_recv.read(&mut buf).await {
            if cli_send.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = cli_send.finish();
    };

    tokio::join!(c2b, b2c);
    Ok(())
}

async fn read_frame(recv: &mut quinn::RecvStream) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 1024 * 1024 {
        anyhow::bail!("frame too large: {len}");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_frame(send: &mut quinn::SendStream, msg: &serde_json::Value) -> anyhow::Result<()> {
    let data = serde_json::to_vec(msg)?;
    let len = (data.len() as u32).to_le_bytes();
    send.write_all(&len).await?;
    send.write_all(&data).await?;
    Ok(())
}

fn load_server_tls(
    cert_path: &str,
    key_path: &str,
) -> anyhow::Result<quinn::crypto::rustls::QuicServerConfig> {
    let cert_pem = std::fs::read(cert_path).with_context(|| format!("read cert: {cert_path}"))?;
    let key_pem = std::fs::read(key_path).with_context(|| format!("read key: {key_path}"))?;

    let certs: Vec<_> = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<Vec<_>, _>>()
        .context("parse cert PEM")?;
    if certs.is_empty() {
        anyhow::bail!("no certificates in {cert_path}");
    }

    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .context("parse key PEM")?
        .context("no private key found")?;

    let mut rustls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("build TLS config: {e}"))?;
    // Transitional dual ALPN: accept "clum" plus legacy "yunying" so pre-0.10
    // bridges keep connecting. Drop b"yunying" once all bridges are upgraded.
    rustls_config.alpn_protocols = vec![b"clum".to_vec(), b"yunying".to_vec()];

    quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_config))
        .map_err(|e| anyhow::anyhow!("QUIC crypto config: {e}"))
}
