//! QUIC transport layer for connecting the MCP server to remote rmux-bridge
//! instances. Requires CA-verified TLS handshakes and token-based authentication.
//!
//! Endpoint/transport configuration and the AUTH handshake live in
//! `clum_core::quic`; this module adds MCP-specific concerns (JSON protocol
//! stream, retry, registry-aware routing).

use anyhow::{Context, Result};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::sleep;

/// 文件传输/长连接使用的 idle timeout（1 小时）。
const FILE_IDLE_TIMEOUT: Duration = Duration::from_secs(3600);
/// QUIC 握手超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Establish QUIC connection to bridge and open the JSON protocol stream.
/// Returns authenticated Connection + json stream's send/recv handles.
/// Use `host.bridge_addr` directly — TCP and UDP share port 9778 safely.
pub async fn connect_to_bridge_quic(
    bridge_addr: &str,
    auth_token: &str,
    ca_cert_path: Option<&str>,
) -> anyhow::Result<(quinn::Connection, quinn::SendStream, quinn::RecvStream)> {
    let conn = clum_core::quic::connect_bridge(
        bridge_addr,
        auth_token,
        ca_cert_path,
        FILE_IDLE_TIMEOUT,
        CONNECT_TIMEOUT,
    )
    .await?;
    tracing::info!("QUIC connected and authenticated to {}", bridge_addr);
    let (send, recv) = open_json_bi(&conn).await?;
    Ok((conn, send, recv))
}

/// Establish QUIC connection to bridge for long-lived forwards.
/// Uses 1-hour idle timeout + 15s keepalive to prevent connection drops.
/// Returns the authenticated Connection (auth stream already finished).
pub async fn connect_to_bridge_quic_forward(
    bridge_addr: &str,
    auth_token: &str,
    ca_cert_path: Option<&str>,
) -> anyhow::Result<quinn::Connection> {
    let conn = clum_core::quic::connect_bridge(
        bridge_addr,
        auth_token,
        ca_cert_path,
        FILE_IDLE_TIMEOUT,
        CONNECT_TIMEOUT,
    )
    .await?;
    tracing::info!(
        "QUIC forward connected and authenticated to {}",
        bridge_addr
    );
    Ok(conn)
}

/// Like [`connect_to_bridge_quic`] with caller-chosen idle/keepalive tuning
/// (used by stream_pane and recording sync).
pub async fn connect_to_bridge_quic_stream(
    bridge_addr: &str,
    auth_token: &str,
    ca_cert_path: Option<&str>,
    idle_timeout_secs: u64,
    keepalive_secs: u64,
) -> anyhow::Result<(quinn::Connection, quinn::SendStream, quinn::RecvStream)> {
    let addr: std::net::SocketAddr = bridge_addr
        .parse()
        .with_context(|| format!("invalid bridge address: {bridge_addr}"))?;
    let endpoint = clum_core::quic::client_endpoint(
        ca_cert_path,
        &[],
        Duration::from_secs(idle_timeout_secs),
        Duration::from_secs(keepalive_secs),
    )?;
    let server_name = bridge_addr.split(':').next().unwrap_or("localhost");
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect(addr, server_name)?)
        .await
        .context("QUIC connect timeout")?
        .context("QUIC connection failed")?;
    clum_core::quic::authenticate_bridge(&conn, auth_token).await?;
    tracing::info!("QUIC stream connected and authenticated to {}", bridge_addr);
    let (send, recv) = open_json_bi(&conn).await?;
    Ok((conn, send, recv))
}

/// Open a bidi stream and mark it as JSON protocol (0x01 magic byte).
async fn open_json_bi(
    conn: &quinn::Connection,
) -> anyhow::Result<(quinn::SendStream, quinn::RecvStream)> {
    let (mut send, recv) = conn
        .open_bi()
        .await
        .context("failed to open QUIC json stream")?;
    send.write_all(&[0x01]).await?;
    Ok((send, recv))
}

// ══════════════════════════════════════════════════════════════════
// QUIC transport
// ══════════════════════════════════════════════════════════════════

pub enum BridgeStream {
    Quic {
        #[allow(dead_code)]
        conn: quinn::Connection,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
    },
}

impl AsyncRead for BridgeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            BridgeStream::Quic { recv, .. } => Pin::new(recv).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for BridgeStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            BridgeStream::Quic { send, .. } => match Pin::new(send).poll_write(cx, buf) {
                Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
                Poll::Ready(Err(e)) => {
                    Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e)))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            BridgeStream::Quic { .. } => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            BridgeStream::Quic { send, .. } => Pin::new(send).poll_shutdown(cx),
        }
    }
}

/// Exponential-backoff retry wrapper (500ms base, doubling).
async fn with_retry<T, F, Fut>(label: &str, max_retries: u32, mut connect: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut attempt = 0;
    loop {
        match connect().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < max_retries => {
                attempt += 1;
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                tracing::warn!(
                    "{label} connect failed (attempt {attempt}/{max_retries}), retrying in {delay:?}: {e}"
                );
                sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// QUIC connection with retry.
pub async fn connect_to_bridge_hybrid(
    bridge_addr: &str,
    auth_token: &str,
    ca_cert_path: Option<&str>,
    max_retries: u32,
) -> Result<BridgeStream> {
    with_retry("QUIC", max_retries, || async {
        let (conn, send, recv) =
            connect_to_bridge_quic(bridge_addr, auth_token, ca_cert_path).await?;
        tracing::info!("connected via QUIC to {}", bridge_addr);
        Ok(BridgeStream::Quic { conn, send, recv })
    })
    .await
}

pub async fn send_json_frame<S: tokio::io::AsyncWriteExt + Unpin>(
    stream: &mut S,
    value: &serde_json::Value,
) -> anyhow::Result<()> {
    let json_str = serde_json::to_string(value)?;
    let len = json_str.len() as u32;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(json_str.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn recv_json_frame<S: tokio::io::AsyncReadExt + Unpin>(
    stream: &mut S,
) -> anyhow::Result<serde_json::Value> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > clum_core::MAX_FRAME_SIZE {
        anyhow::bail!(
            "frame too large: {} bytes (max {})",
            len,
            clum_core::MAX_FRAME_SIZE
        );
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

pub async fn connect_to_bridge_hybrid_stream(
    bridge_addr: &str,
    auth_token: &str,
    ca_cert_path: Option<&str>,
    max_retries: u32,
    idle_timeout_secs: u64,
    keepalive_secs: u64,
) -> Result<BridgeStream> {
    with_retry("QUIC stream", max_retries, || async {
        let (conn, send, recv) = connect_to_bridge_quic_stream(
            bridge_addr,
            auth_token,
            ca_cert_path,
            idle_timeout_secs,
            keepalive_secs,
        )
        .await?;
        tracing::info!("connected via QUIC stream to {}", bridge_addr);
        Ok(BridgeStream::Quic { conn, send, recv })
    })
    .await
}

// ══════════════════════════════════════════════════════════════════
// Registry-aware connection (central server mode)
// ══════════════════════════════════════════════════════════════════

pub async fn connect_to_host(
    ctx: &crate::tools::ToolContext,
    host: &clum_core::types::HostConfig,
) -> Result<BridgeStream> {
    connect_via_registry(&ctx.bridge_registry, host, ctx.ca_cert_path.as_deref()).await
}

pub async fn connect_to_host_stream(
    ctx: &crate::tools::ToolContext,
    host: &clum_core::types::HostConfig,
    idle_timeout_secs: u64,
    keepalive_secs: u64,
) -> Result<BridgeStream> {
    if let Some(stream) = try_registry_stream(&ctx.bridge_registry, &host.name).await {
        return Ok(stream);
    }
    match (&host.bridge_addr, &host.bridge_token) {
        (Some(addr), Some(token)) => {
            connect_to_bridge_hybrid_stream(
                addr,
                token,
                ctx.ca_cert_path.as_deref(),
                3,
                idle_timeout_secs,
                keepalive_secs,
            )
            .await
        }
        _ => anyhow::bail!(
            "host '{}': no bridge_addr/bridge_token configured and not found in registry — \
             either configure hosts.yaml for direct connection or ensure bridge is enrolled",
            host.name
        ),
    }
}

pub async fn connect_via_registry(
    registry: &std::sync::Arc<crate::registry::BridgeRegistry>,
    host: &clum_core::types::HostConfig,
    ca_cert_path: Option<&str>,
) -> Result<BridgeStream> {
    if let Some(stream) = try_registry_stream(registry, &host.name).await {
        return Ok(stream);
    }
    match (&host.bridge_addr, &host.bridge_token) {
        (Some(addr), Some(token)) => connect_to_bridge_hybrid(addr, token, ca_cert_path, 3).await,
        _ => anyhow::bail!(
            "host '{}': no bridge_addr/bridge_token configured and not found in registry — \
             either configure hosts.yaml for direct connection or ensure bridge is enrolled",
            host.name
        ),
    }
}

/// Try routing through a reverse-registered bridge connection; None means
/// fall back to direct connection.
async fn try_registry_stream(
    registry: &std::sync::Arc<crate::registry::BridgeRegistry>,
    host_name: &str,
) -> Option<BridgeStream> {
    let bridge = registry.get(host_name).await?;
    if bridge.conn.close_reason().is_some() {
        return None;
    }
    match open_json_stream(&bridge.conn).await {
        Ok(stream) => {
            tracing::debug!("routed via registry to {}", host_name);
            Some(stream)
        }
        Err(e) => {
            tracing::warn!(
                "registry stream to {} failed, falling back: {}",
                host_name,
                e
            );
            None
        }
    }
}

async fn open_json_stream(conn: &quinn::Connection) -> Result<BridgeStream> {
    let (send, recv) = open_json_bi(conn)
        .await
        .context("open_bi on registered conn")?;
    Ok(BridgeStream::Quic {
        conn: conn.clone(),
        send,
        recv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::AsyncWriteExt;

    #[test]
    fn constants_file_idle_timeout() {
        assert_eq!(FILE_IDLE_TIMEOUT, Duration::from_secs(3600));
        assert_eq!(FILE_IDLE_TIMEOUT.as_secs(), 3600);
    }

    #[test]
    fn constants_connect_timeout() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(CONNECT_TIMEOUT.as_secs(), 10);
    }

    #[tokio::test]
    async fn with_retry_succeeds_first_try() {
        let result = with_retry("test", 3, || async { Ok::<i32, anyhow::Error>(42) }).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn with_retry_succeeds_after_failures() {
        let counter = AtomicU32::new(0);
        let result = with_retry("test", 3, || {
            let c = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if c < 2 {
                    anyhow::bail!("attempt {c} failed")
                } else {
                    Ok(c)
                }
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 2);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn with_retry_all_failures_returns_last_error() {
        let result: anyhow::Result<i32> =
            with_retry("test", 2, || async { anyhow::bail!("always fail") }).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("always fail"));
    }

    #[tokio::test]
    async fn with_retry_zero_max_retries_fails_immediately() {
        let result: anyhow::Result<i32> =
            with_retry("test", 0, || async { anyhow::bail!("no retries") }).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("no retries"));
    }

    #[tokio::test]
    async fn send_recv_json_roundtrip_simple() {
        let (client, server) = tokio::io::duplex(4096);
        let (mut _cr, mut cw) = tokio::io::split(client);
        let (mut sr, mut _sw) = tokio::io::split(server);

        let value = serde_json::json!({"hello": "world", "num": 42});
        let (send_res, recv_res) =
            tokio::join!(send_json_frame(&mut cw, &value), recv_json_frame(&mut sr));

        assert!(send_res.is_ok(), "send failed: {:?}", send_res.err());
        assert!(recv_res.is_ok(), "recv failed: {:?}", recv_res.err());
        assert_eq!(recv_res.unwrap(), value);
    }

    #[tokio::test]
    async fn send_recv_json_roundtrip_empty_object() {
        let (client, server) = tokio::io::duplex(4096);
        let (mut _cr, mut cw) = tokio::io::split(client);
        let (mut sr, mut _sw) = tokio::io::split(server);

        let value = serde_json::json!({});
        let (send_res, recv_res) =
            tokio::join!(send_json_frame(&mut cw, &value), recv_json_frame(&mut sr));

        assert!(send_res.is_ok());
        assert!(recv_res.is_ok());
        assert_eq!(recv_res.unwrap(), value);
    }

    #[tokio::test]
    async fn send_recv_json_roundtrip_null() {
        let (client, server) = tokio::io::duplex(4096);
        let (mut _cr, mut cw) = tokio::io::split(client);
        let (mut sr, mut _sw) = tokio::io::split(server);

        let value = serde_json::Value::Null;
        let (send_res, recv_res) =
            tokio::join!(send_json_frame(&mut cw, &value), recv_json_frame(&mut sr));

        assert!(send_res.is_ok());
        assert!(recv_res.is_ok());
        assert_eq!(recv_res.unwrap(), value);
    }

    #[tokio::test]
    #[allow(clippy::approx_constant)]
    async fn send_recv_json_roundtrip_nested() {
        let (client, server) = tokio::io::duplex(8192);
        let (mut _cr, mut cw) = tokio::io::split(client);
        let (mut sr, mut _sw) = tokio::io::split(server);

        let value = serde_json::json!({
            "array": [1, "two", null, true, {"deep": [3.14]}],
            "escaped": "line1\nline2\t\"quoted\""
        });
        let (send_res, recv_res) =
            tokio::join!(send_json_frame(&mut cw, &value), recv_json_frame(&mut sr));

        assert!(send_res.is_ok());
        assert!(recv_res.is_ok());
        assert_eq!(recv_res.unwrap(), value);
    }

    #[tokio::test]
    async fn recv_json_frame_rejects_oversized_frame() {
        let (mut client, mut server) = tokio::io::duplex(64);

        let fake_len = (clum_core::MAX_FRAME_SIZE + 1) as u32;
        client.write_all(&fake_len.to_le_bytes()).await.unwrap();
        drop(client);

        let result = recv_json_frame(&mut server).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("frame too large"), "unexpected error: {msg}");
    }
}
