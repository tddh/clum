use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use sha2::{Digest, Sha256};

use crate::registry::{BridgeConn, BridgeRegistry};

pub struct QuicServerConfig {
    pub listen_addr: String,
    pub cert_path: String,
    pub key_path: String,
    /// SHA-256(token) hex → hostname
    pub bridge_token_hashes: HashMap<String, String>,
    pub recordings_dir: std::path::PathBuf,
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

    let addr: SocketAddr = config
        .listen_addr
        .parse()
        .context("invalid QUIC listen addr")?;
    let endpoint = quinn::Endpoint::server(server_config, addr)?;
    tracing::info!("QUIC server listening on udp/{addr} (ALPN: yunying)");

    let token_map = Arc::new(config.bridge_token_hashes);

    while let Some(incoming) = endpoint.accept().await {
        let registry = Arc::clone(&registry);
        let token_map = Arc::clone(&token_map);
        let rec_dir = config.recordings_dir.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming, registry, token_map, rec_dir).await {
                tracing::debug!("QUIC connection handler ended: {e}");
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    incoming: quinn::Incoming,
    registry: Arc<BridgeRegistry>,
    token_map: Arc<HashMap<String, String>>,
    recordings_dir: std::path::PathBuf,
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
            )
            .await
        }
        Some("agent_connect") => {
            handle_agent_connection(conn, remote_addr, send, recv, msg, registry).await
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
    token_map: Arc<HashMap<String, String>>,
    recordings_dir: std::path::PathBuf,
) -> anyhow::Result<()> {
    let token = reg
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let token_hash = hex::encode(Sha256::digest(token.as_bytes()));

    let hostname = match token_map.get(&token_hash) {
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
                tokio::spawn(async move {
                    if let Err(e) = handle_push_stream(push_send, push_recv, &dir, &host).await {
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
) -> anyhow::Result<()> {
    let header_data = read_frame(&mut recv).await?;
    let header: serde_json::Value = serde_json::from_slice(&header_data)?;

    if header.get("type").and_then(|v| v.as_str()) != Some("recording_push") {
        anyhow::bail!("unexpected push type");
    }

    let filename = header
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown.cast");
    let size = header.get("size").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

    let host_dir = recordings_dir.join(hostname);
    tokio::fs::create_dir_all(&host_dir).await?;
    let file_path = host_dir.join(filename);

    let mut file_data = vec![0u8; size];
    recv.read_exact(&mut file_data).await?;
    tokio::fs::write(&file_path, &file_data).await?;

    tracing::info!(%hostname, %filename, size, "recording received");

    let ack = serde_json::json!({"type": "recording_ack", "ok": true});
    let ack_data = serde_json::to_vec(&ack)?;
    let len = (ack_data.len() as u32).to_le_bytes();
    send.write_all(&len).await?;
    send.write_all(&ack_data).await?;

    Ok(())
}

async fn handle_agent_connection(
    conn: quinn::Connection,
    remote_addr: std::net::SocketAddr,
    mut send: quinn::SendStream,
    _recv: quinn::RecvStream,
    msg: serde_json::Value,
    registry: Arc<BridgeRegistry>,
) -> anyhow::Result<()> {
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

    let bridge = match registry.get(&host).await {
        Some(b) => b,
        None => {
            write_frame(&mut send, &serde_json::json!({"type": "agent_ack", "ok": false, "error": format!("host '{host}' not online")})).await?;
            anyhow::bail!("agent requested offline host '{host}' from {remote_addr}");
        }
    };

    write_frame(
        &mut send,
        &serde_json::json!({"type": "agent_ack", "ok": true, "host": host}),
    )
    .await?;
    tracing::info!(%host, %remote_addr, "agent connected, starting relay");

    // Relay loop: for each stream CLI opens, open corresponding stream to Bridge and relay
    loop {
        match conn.accept_bi().await {
            Ok((cli_send, cli_recv)) => {
                let bridge_conn = bridge.conn.clone();
                tokio::spawn(async move {
                    if let Err(e) = relay_stream(cli_send, cli_recv, &bridge_conn).await {
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
        let mut buf = [0u8; 8192];
        while let Ok(Some(n)) = cli_recv.read(&mut buf).await {
            if bridge_send.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = bridge_send.finish();
    };

    let b2c = async {
        let mut buf = [0u8; 8192];
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
    rustls_config.alpn_protocols = vec![b"yunying".to_vec()];

    quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_config))
        .map_err(|e| anyhow::anyhow!("QUIC crypto config: {e}"))
}
