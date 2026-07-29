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
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming, registry, token_map).await {
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
) -> anyhow::Result<()> {
    let conn = incoming.await?;
    let remote_addr = conn.remote_address();

    let (mut send, mut recv) = conn.accept_bi().await?;

    let reg_msg = read_frame(&mut recv).await?;
    let reg: serde_json::Value = serde_json::from_slice(&reg_msg)?;

    if reg.get("type").and_then(|v| v.as_str()) != Some("bridge_register") {
        write_frame(
            &mut send,
            &serde_json::json!({
                "type": "register_ack",
                "ok": false,
                "error": "expected bridge_register"
            }),
        )
        .await?;
        conn.close(quinn::VarInt::from_u32(0), b"protocol error");
        anyhow::bail!("unexpected first message from {remote_addr}");
    }

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

    while let Ok(data) = read_frame(&mut recv).await {
        let msg: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if msg.get("type").and_then(|v| v.as_str()) == Some("ping") {
            registry.update_heartbeat(&hostname).await;
        }
    }

    registry.unregister(&hostname).await;
    tracing::info!(%hostname, "bridge disconnected");
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
