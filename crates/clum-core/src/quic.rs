//! Shared QUIC client/server transport primitives for the clum ecosystem.
//!
//! All crates (clum-mcp, clum-cli, rmux-bridge) use these helpers so the
//! transport parameters (flow-control windows, congestion control, keepalive)
//! stay consistent instead of drifting across copy-pasted implementations.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

/// 16 MB 流控窗口：quinn 默认初始拥塞窗口 ~12 KB，内网千兆链路下
/// 慢启动要 ~20 个 RTT 才能打满带宽；调大窗口大幅缩短爬坡时间。
pub const WINDOW_SIZE: u32 = 16 * 1024 * 1024;

/// 所有长连接共用的 keep-alive 间隔。
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(15);

/// bridge 握手成功时返回的应答。
const AUTH_OK: &[u8; 3] = b"OK\n";

/// 构建统一的 QUIC 传输配置（窗口/拥塞控制/keepalive/idle）。
pub fn build_transport_config(
    idle_timeout: Duration,
    keepalive: Duration,
) -> anyhow::Result<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        idle_timeout
            .try_into()
            .map_err(|_| anyhow::anyhow!("idle timeout exceeds QUIC VarInt limit"))?,
    ));
    transport.keep_alive_interval(Some(keepalive));
    transport.stream_receive_window(quinn::VarInt::from_u32(WINDOW_SIZE));
    transport.send_window(WINDOW_SIZE as u64);
    transport.receive_window(quinn::VarInt::from_u32(WINDOW_SIZE));
    transport.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    Ok(transport)
}

/// 构建 QUIC 客户端 TLS 加密层。`alpn` 为空时不设置 ALPN（bridge 直连）。
pub fn build_client_crypto(
    ca_cert_path: Option<&str>,
    alpn: &[&[u8]],
) -> anyhow::Result<Arc<quinn::crypto::rustls::QuicClientConfig>> {
    let root_store = crate::build_root_store(ca_cert_path)?;
    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls_config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls_config))
        .map_err(|e| anyhow::anyhow!("QUIC TLS config error: {e}"))?;
    Ok(Arc::new(crypto))
}

/// 创建带统一传输参数的 QUIC 客户端 endpoint。
pub fn client_endpoint(
    ca_cert_path: Option<&str>,
    alpn: &[&[u8]],
    idle_timeout: Duration,
    keepalive: Duration,
) -> anyhow::Result<quinn::Endpoint> {
    let mut endpoint = quinn::Endpoint::client("[::]:0".parse()?)?;
    let mut client_config = quinn::ClientConfig::new(build_client_crypto(ca_cert_path, alpn)?);
    client_config.transport_config(Arc::new(build_transport_config(idle_timeout, keepalive)?));
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

/// 在已建立的连接上完成 bridge token AUTH 握手。
///
/// 错误消息包含 "authentication failed"，MCP 侧错误分类依赖该子串。
pub async fn authenticate_bridge(conn: &quinn::Connection, auth_token: &str) -> anyhow::Result<()> {
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .context("failed to open QUIC auth stream")?;
    let token_bytes = auth_token.as_bytes();
    send.write_all(b"AUTH").await?;
    send.write_all(&(token_bytes.len() as u32).to_le_bytes())
        .await?;
    send.write_all(token_bytes).await?;
    send.finish()?;

    let mut response = [0u8; 3];
    recv.read_exact(&mut response).await?;
    if &response != AUTH_OK {
        conn.close(1u32.into(), b"auth failed");
        anyhow::bail!("bridge QUIC authentication failed");
    }
    Ok(())
}

/// 连接 bridge（解析地址 → QUIC 握手 → token 认证），返回认证后的连接。
pub async fn connect_bridge(
    bridge_addr: &str,
    auth_token: &str,
    ca_cert_path: Option<&str>,
    idle_timeout: Duration,
    connect_timeout: Duration,
) -> anyhow::Result<quinn::Connection> {
    let addr: std::net::SocketAddr = bridge_addr
        .parse()
        .with_context(|| format!("invalid bridge address: {bridge_addr}"))?;
    let endpoint = client_endpoint(ca_cert_path, &[], idle_timeout, DEFAULT_KEEPALIVE)?;
    let server_name = bridge_addr.split(':').next().unwrap_or("localhost");
    let conn = tokio::time::timeout(connect_timeout, endpoint.connect(addr, server_name)?)
        .await
        .context("QUIC connect timeout")?
        .context("QUIC connection failed")?;
    authenticate_bridge(&conn, auth_token).await?;
    Ok(conn)
}

/// 从 QUIC 双向流读取长度前缀（LE32）的 JSON 控制帧。
///
/// 控制帧（注册、心跳、工具指令、token 轮换）不应超过 1 MB，
/// 防止异常长度声明导致的内存浪费。
pub async fn read_frame(recv: &mut quinn::RecvStream) -> anyhow::Result<Vec<u8>> {
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

/// 向 QUIC 双向流写入长度前缀（LE32）的 JSON 控制帧。
pub async fn write_frame(
    send: &mut quinn::SendStream,
    msg: &serde_json::Value,
) -> anyhow::Result<()> {
    let data = serde_json::to_vec(msg)?;
    let len = (data.len() as u32).to_le_bytes();
    send.write_all(&len).await?;
    send.write_all(&data).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ensure_crypto_provider() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .expect("failed to install ring crypto provider");
        });
    }

    #[test]
    fn build_transport_config_does_not_panic_standard_params() {
        let _config =
            build_transport_config(Duration::from_secs(30), Duration::from_secs(15)).unwrap();
    }

    #[test]
    fn build_transport_config_does_not_panic_zero_timeout() {
        let _config =
            build_transport_config(Duration::from_secs(0), Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn build_transport_config_does_not_panic_large_timeout() {
        let _config =
            build_transport_config(Duration::from_secs(3600), Duration::from_secs(60)).unwrap();
    }

    #[test]
    fn build_transport_config_does_not_panic_zero_keepalive() {
        let _config =
            build_transport_config(Duration::from_secs(30), Duration::from_secs(0)).unwrap();
    }

    #[test]
    fn window_size_constant_is_16mb() {
        assert_eq!(WINDOW_SIZE, 16 * 1024 * 1024);
    }

    #[test]
    fn default_keepalive_is_15_seconds() {
        assert_eq!(DEFAULT_KEEPALIVE, Duration::from_secs(15));
    }

    #[test]
    fn build_client_crypto_none_ca_uses_webpki_roots() {
        ensure_crypto_provider();
        let result = build_client_crypto(None, &[]);
        assert!(result.is_ok(), "should build crypto with webpki roots");
    }

    #[test]
    fn build_client_crypto_with_alpn_succeeds() {
        ensure_crypto_provider();
        let alpn = &[b"h3" as &[u8]];
        let result = build_client_crypto(None, alpn);
        assert!(result.is_ok(), "should build crypto with ALPN set");
    }

    #[test]
    fn build_client_crypto_nonexistent_ca_returns_error() {
        let result = build_client_crypto(Some("/nonexistent/ca-cert.pem"), &[]);
        assert!(result.is_err(), "nonexistent CA path should return error");
    }

    #[test]
    fn constants_are_nonzero() {
        const { assert!(WINDOW_SIZE > 0) };
        assert!(DEFAULT_KEEPALIVE > Duration::from_secs(0));
    }
}
