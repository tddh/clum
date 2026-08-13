use std::time::Duration;

use anyhow::{Context, Result};
use clum_core::HostConfig;

/// 交互式终端连接的 idle timeout：断线检测靠 keepalive，60s 足够。
const TERM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn connect_via_server(
    server_addr: &str,
    ca_cert_path: Option<&str>,
    host: &str,
    api_key: Option<&str>,
    purpose: &str,
) -> Result<quinn::Connection> {
    let endpoint = clum_core::quic::client_endpoint(
        ca_cert_path,
        &[b"clum"],
        TERM_IDLE_TIMEOUT,
        clum_core::quic::DEFAULT_KEEPALIVE,
    )?;

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
    clum_core::quic::connect_bridge(
        bridge_addr,
        bridge_token,
        ca_cert_path,
        TERM_IDLE_TIMEOUT,
        Duration::from_secs(10),
    )
    .await
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
    lowest_pane_on_conn(&conn, session_name).await
}

/// Central Server 模式下通过 server 中转解析 window 0 的最小 pane_id。
/// 与直连版共享 pane 解析逻辑，避免 term 硬编码 %0 在 pane 编号非 0 时
/// 报 "pane %0 was not found"（例如 %0 曾被 exit 关闭、重建后编号递增）。
pub async fn find_lowest_pane_via_server(
    server_addr: &str,
    ca_cert_path: Option<&str>,
    host: &str,
    api_key: Option<&str>,
    session_name: &str,
) -> Result<String> {
    let conn = connect_via_server(server_addr, ca_cert_path, host, api_key, "term").await?;
    lowest_pane_on_conn(&conn, session_name).await
}

/// Central Server 模式下确保 session 存在：不存在则创建同名 detached session。
/// term 在 pane 解析与 attach 前调用，使 `term <host> --session <name>` 在
/// session 缺失时自动创建而非报错退出。
pub async fn ensure_session_via_server(
    server_addr: &str,
    ca_cert_path: Option<&str>,
    host: &str,
    api_key: Option<&str>,
    session_name: &str,
) -> Result<()> {
    let conn = connect_via_server(server_addr, ca_cert_path, host, api_key, "term").await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[0x01]).await?;

    let has = serde_json::json!({
        "type": "has_session",
        "name": session_name,
    });
    crate::protocol::send_json_frame(&mut send, &has).await?;
    let resp = crate::protocol::recv_json_frame(&mut recv).await?;
    let exists = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if exists {
        return Ok(());
    }

    let create = serde_json::json!({
        "type": "new_session",
        "name": session_name,
        "detached": true,
    });
    crate::protocol::send_json_frame(&mut send, &create).await?;
    let resp = crate::protocol::recv_json_frame(&mut recv).await?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = resp["error"].as_str().unwrap_or("unknown error");
        anyhow::bail!("failed to create session {}: {}", session_name, err);
    }
    Ok(())
}

/// Direct 模式下确保 session 存在（逻辑同 ensure_session_via_server，走 bridge 直连）。
pub async fn ensure_session(
    config: &HostConfig,
    ca_cert_path: Option<&str>,
    session_name: &str,
) -> Result<()> {
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

    let has = serde_json::json!({ "type": "has_session", "name": session_name });
    crate::protocol::send_json_frame(&mut send, &has).await?;
    let resp = crate::protocol::recv_json_frame(&mut recv).await?;
    let exists = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if exists {
        return Ok(());
    }

    let create =
        serde_json::json!({ "type": "new_session", "name": session_name, "detached": true });
    crate::protocol::send_json_frame(&mut send, &create).await?;
    let resp = crate::protocol::recv_json_frame(&mut recv).await?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = resp["error"].as_str().unwrap_or("unknown error");
        anyhow::bail!("failed to create session {}: {}", session_name, err);
    }
    Ok(())
}

async fn lowest_pane_on_conn(conn: &quinn::Connection, session_name: &str) -> Result<String> {
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
