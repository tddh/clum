use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clum_core::HostConfig;

const QUIC_WINDOW_SIZE: u32 = 16 * 1024 * 1024;

fn build_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(60).try_into().unwrap()));
    transport.keep_alive_interval(Some(Duration::from_secs(15)));
    transport.stream_receive_window(quinn::VarInt::from_u32(QUIC_WINDOW_SIZE));
    transport.send_window(QUIC_WINDOW_SIZE as u64);
    transport.receive_window(quinn::VarInt::from_u32(QUIC_WINDOW_SIZE));
    transport.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    transport
}

pub async fn connect_via_server(
    server_addr: &str,
    ca_cert_path: Option<&str>,
    host: &str,
    api_key: Option<&str>,
    purpose: &str,
) -> Result<quinn::Connection> {
    let roots = clum_core::build_root_store(ca_cert_path)?;

    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"clum".to_vec()];

    let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
        .map_err(|e| anyhow::anyhow!("QUIC TLS config error: {}", e))?;

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_tls));
    client_config.transport_config(Arc::new(build_transport_config()));
    endpoint.set_default_client_config(client_config);

    let addr: std::net::SocketAddr = server_addr
        .parse()
        .or_else(|_| Ok::<_, anyhow::Error>(format!("{server_addr}:9788").parse()?))?;
    let server_name = server_addr.split(':').next().unwrap_or("localhost");

    let conn = endpoint
        .connect(addr, server_name)?
        .await
        .context("QUIC handshake to server failed")?;

    let (mut send, mut recv) = conn.open_bi().await?;

    let msg = serde_json::json!({
        "type": "agent_connect",
        "host": host,
        "api_key": api_key.unwrap_or(""),
        "purpose": purpose,
    });
    let data = serde_json::to_vec(&msg)?;
    let len = (data.len() as u32).to_le_bytes();
    send.write_all(&len).await?;
    send.write_all(&data).await?;

    let mut ack_len_buf = [0u8; 4];
    recv.read_exact(&mut ack_len_buf).await?;
    let ack_len = u32::from_le_bytes(ack_len_buf) as usize;
    let mut ack_buf = vec![0u8; ack_len];
    recv.read_exact(&mut ack_buf).await?;
    let ack: serde_json::Value = serde_json::from_slice(&ack_buf)?;

    if ack.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = ack
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        anyhow::bail!("server rejected connection: {err}");
    }

    Ok(conn)
}

pub async fn connect_to_bridge_quic(
    bridge_addr: &str,
    bridge_token: &str,
    ca_cert_path: Option<&str>,
) -> Result<quinn::Connection> {
    let roots = clum_core::build_root_store(ca_cert_path)?;

    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
        .map_err(|e| anyhow::anyhow!("QUIC TLS config error: {}", e))?;

    let transport = build_transport_config();

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_tls));
    client_config.transport_config(Arc::new(transport));
    endpoint.set_default_client_config(client_config);

    let server_name = bridge_addr.split(':').next().unwrap_or(bridge_addr);
    let conn = endpoint.connect(bridge_addr.parse()?, server_name)?.await?;

    let (mut auth_send, mut auth_recv) = conn.open_bi().await?;
    auth_send.write_all(b"AUTH").await?;
    auth_send
        .write_all(&(bridge_token.len() as u32).to_le_bytes())
        .await?;
    auth_send.write_all(bridge_token.as_bytes()).await?;
    auth_send.finish()?;

    let mut response = [0u8; 32];
    let n = auth_recv.read(&mut response).await?.unwrap_or(0);
    if n < 2 || &response[..n] != b"OK\n" {
        anyhow::bail!("bridge auth failed");
    }

    Ok(conn)
}

pub async fn find_lowest_pane(
    config: &HostConfig,
    ca_cert_path: Option<&str>,
    session_name: &str,
) -> Result<String> {
    let addr = config
        .bridge_addr
        .as_deref()
        .context("bridge_addr not configured")?;
    let token = config
        .bridge_token
        .as_deref()
        .context("bridge_token not configured")?;
    let conn = connect_to_bridge_quic(addr, token, ca_cert_path).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[0x01]).await?;

    let request = serde_json::json!({
        "type": "list_window_panes",
        "session_name": session_name,
        "window_index": 0,
    });
    crate::protocol::send_json_frame(&mut send, &request).await?;
    let response = crate::protocol::recv_json_frame(&mut recv).await?;

    if !response
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let err = response["error"].as_str().unwrap_or("unknown error");
        anyhow::bail!("failed to list panes: {}", err);
    }

    let panes = response
        .get("panes")
        .and_then(|p| p.as_array())
        .context("no panes in response")?;

    let smallest = panes
        .iter()
        .filter_map(|p| p.get("pane_id").and_then(|id| id.as_str()))
        .filter_map(|id| id.trim_start_matches('%').parse::<u32>().ok())
        .min()
        .context("no panes found in session")?;

    Ok(format!("%{}", smallest))
}
