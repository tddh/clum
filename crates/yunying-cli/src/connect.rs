use std::sync::Arc;

use anyhow::{Context, Result};
use yunying_core::HostConfig;

pub async fn connect_via_server(
    server_addr: &str,
    ca_cert_path: &str,
    host: &str,
    api_key: Option<&str>,
) -> Result<quinn::Connection> {
    let ca_pem = std::fs::read(ca_cert_path)
        .with_context(|| format!("failed to read CA cert: {}", ca_cert_path))?;

    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_slice()) {
        let cert = cert?;
        roots.add(cert)?;
    }

    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"yunying".to_vec()];

    let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
        .map_err(|e| anyhow::anyhow!("QUIC TLS config error: {}", e))?;

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    let client_config = quinn::ClientConfig::new(Arc::new(quic_tls));
    endpoint.set_default_client_config(client_config);

    let addr: std::net::SocketAddr = server_addr
        .parse()
        .or_else(|_| Ok::<_, anyhow::Error>(format!("{server_addr}:9778").parse()?))?;
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
    ca_cert_path: &str,
) -> Result<quinn::Connection> {
    let ca_pem = std::fs::read(ca_cert_path)
        .with_context(|| format!("failed to read CA cert: {}", ca_cert_path))?;

    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_slice()) {
        let cert = cert?;
        roots.add(cert)?;
    }

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
        .map_err(|e| anyhow::anyhow!("QUIC TLS config error: {}", e))?;

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(quinn::IdleTimeout::from(quinn::VarInt::from_u32(0))));

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

pub async fn list_sessions(config: &HostConfig, ca_cert_path: &str) -> Result<()> {
    let conn =
        connect_to_bridge_quic(&config.bridge_addr, &config.bridge_token, ca_cert_path).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[0x01]).await?;

    let request = serde_json::json!({ "type": "list_sessions" });
    crate::protocol::send_json_frame(&mut send, &request).await?;
    let response = crate::protocol::recv_json_frame(&mut recv).await?;

    if response.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = response["error"].as_str().unwrap_or("unknown error");
        anyhow::bail!("failed to list sessions: {}", err);
    }

    if let Some(sessions) = response.get("sessions").and_then(|s| s.as_array()) {
        if sessions.is_empty() {
            println!("No active sessions on {}", config.name);
            return Ok(());
        }
        println!("{:<30} HOST", "SESSION");
        println!("{}", "-".repeat(50));
        for session in sessions {
            println!(
                "{:<30} {}",
                session["session_name"].as_str().unwrap_or("-"),
                config.name,
            );
        }
        println!("\n{} session(s) on {}", sessions.len(), config.name);
    } else {
        println!("No sessions found on {}", config.name);
    }

    Ok(())
}

pub async fn find_lowest_pane(
    config: &HostConfig,
    ca_cert_path: &str,
    session_name: &str,
) -> Result<String> {
    let conn =
        connect_to_bridge_quic(&config.bridge_addr, &config.bridge_token, ca_cert_path).await?;
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
