//! Shared QUIC client/server transport primitives for the clum ecosystem.
//!
//! All crates (clum-mcp, clum-cli, rmux-bridge) use these helpers so the
//! transport parameters (flow-control windows, congestion control, keepalive)
//! stay consistent instead of drifting across copy-pasted implementations.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

/// 拥塞控制算法选择。
///
/// `Auto` 为默认行为：按连接目标地址自动判定——内网用 BBR 追求最大吞吐，
/// 公网用 CUBIC 在丢包时主动退避，避免带宽打满导致 QUIC 断连。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcKind {
    Auto,
    Bbr,
    Cubic,
}

impl CcKind {
    /// 解析为具体算法。`Auto` 时按目标地址判定；无目标地址时回退 `Bbr`。
    pub fn resolve(self, target: Option<SocketAddr>) -> CcKind {
        match self {
            CcKind::Auto => match target {
                Some(addr) if is_private_network(addr) => CcKind::Bbr,
                Some(_) => CcKind::Cubic,
                None => CcKind::Bbr,
            },
            other => other,
        }
    }

    /// 从环境变量解析，非法或未设置时回退 `Auto`。
    pub fn from_env(name: &str) -> CcKind {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<CcKind>().ok())
            .unwrap_or(CcKind::Auto)
    }

    /// 返回对应算法的 `ControllerFactory`。`Auto` 直接调用时回退 `Bbr`。
    pub fn factory(self) -> Arc<dyn quinn::congestion::ControllerFactory + Send + Sync> {
        match self {
            CcKind::Bbr => Arc::new(quinn::congestion::BbrConfig::default()),
            CcKind::Cubic => Arc::new(quinn::congestion::CubicConfig::default()),
            CcKind::Auto => Arc::new(quinn::congestion::BbrConfig::default()),
        }
    }
}

impl std::str::FromStr for CcKind {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(CcKind::Auto),
            "bbr" => Ok(CcKind::Bbr),
            "cubic" => Ok(CcKind::Cubic),
            _ => Err("expected auto|bbr|cubic"),
        }
    }
}

/// 判断目标地址是否为内网（RFC1918 / loopback / link-local / IPv6 ULA）。
pub fn is_private_network(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_unique_local() || ip.is_loopback(),
    }
}

/// 16 MB 流控窗口：quinn 默认初始拥塞窗口 ~12 KB，内网千兆链路下
/// 慢启动要 ~20 个 RTT 才能打满带宽；调大窗口大幅缩短爬坡时间。
pub const WINDOW_SIZE: u32 = 16 * 1024 * 1024;

/// UDP socket 收发缓冲（SO_SNDBUF/SO_RCVBUF）目标值。
///
/// quinn 创建 socket 时**不设置** buffer，用系统默认值（Linux 208KB、
/// macOS 发送 9KB），高速传输时内核缓冲被打满 → ENOBUFS 丢包 → QUIC
/// 重传 → 吞吐骤降（表现为大文件下载卡顿）。此值需与内核上限
/// `net.core.wmem_max/rmem_max` 匹配（Linux 默认 208KB 会 clamp，需
/// sysctl 调大），否则 setsockopt 无效。
pub const UDP_BUFFER_SIZE: usize = 16 * 1024 * 1024;

/// 创建带大收发缓冲的 UDP socket。
///
/// quinn 的 `Endpoint::client`/`Endpoint::server` 直接用系统默认 socket，
/// 无法设置 SO_SNDBUF/SO_RCVBUF。本函数用 socket2 创建并配置大缓冲后，
/// 经 `quinn::Endpoint::new` 交给 quinn，避免高吞吐时内核缓冲溢出丢包。
pub fn build_udp_socket(addr: SocketAddr) -> anyhow::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(false)?;
    }
    socket.set_recv_buffer_size(UDP_BUFFER_SIZE)?;
    socket.set_send_buffer_size(UDP_BUFFER_SIZE)?;
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

/// 所有长连接共用的 keep-alive 间隔。
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(15);

/// bridge 握手成功时返回的应答。
const AUTH_OK: &[u8; 3] = b"OK\n";

/// 构建统一的 QUIC 传输配置（窗口/拥塞控制/keepalive/idle）。
pub fn build_transport_config(
    idle_timeout: Duration,
    keepalive: Duration,
    cc: CcKind,
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
    transport.congestion_controller_factory(cc.factory());
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
    cc: CcKind,
) -> anyhow::Result<quinn::Endpoint> {
    let socket = build_udp_socket("[::]:0".parse()?)?;
    let runtime = quinn::default_runtime().context("no async runtime found")?;
    let mut endpoint =
        quinn::Endpoint::new(quinn::EndpointConfig::default(), None, socket, runtime)?;
    let mut client_config = quinn::ClientConfig::new(build_client_crypto(ca_cert_path, alpn)?);
    client_config.transport_config(Arc::new(build_transport_config(
        idle_timeout,
        keepalive,
        cc,
    )?));
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
    cc: CcKind,
) -> anyhow::Result<quinn::Connection> {
    let addr: std::net::SocketAddr = bridge_addr
        .parse()
        .with_context(|| format!("invalid bridge address: {bridge_addr}"))?;
    let cc = cc.resolve(Some(addr));
    let endpoint = client_endpoint(ca_cert_path, &[], idle_timeout, DEFAULT_KEEPALIVE, cc)?;
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
        let _config = build_transport_config(
            Duration::from_secs(30),
            Duration::from_secs(15),
            CcKind::Bbr,
        )
        .unwrap();
    }

    #[test]
    fn build_transport_config_does_not_panic_zero_timeout() {
        let _config =
            build_transport_config(Duration::from_secs(0), Duration::from_secs(5), CcKind::Bbr)
                .unwrap();
    }

    #[test]
    fn build_transport_config_does_not_panic_large_timeout() {
        let _config = build_transport_config(
            Duration::from_secs(3600),
            Duration::from_secs(60),
            CcKind::Bbr,
        )
        .unwrap();
    }

    #[test]
    fn build_transport_config_does_not_panic_zero_keepalive() {
        let _config =
            build_transport_config(Duration::from_secs(30), Duration::from_secs(0), CcKind::Bbr)
                .unwrap();
    }

    #[test]
    fn build_transport_config_accepts_cubic() {
        let _config = build_transport_config(
            Duration::from_secs(30),
            Duration::from_secs(15),
            CcKind::Cubic,
        )
        .unwrap();
    }

    #[test]
    fn build_transport_config_accepts_auto_resolved() {
        let cc = CcKind::Auto.resolve(Some("8.8.8.8:443".parse().unwrap()));
        let _config =
            build_transport_config(Duration::from_secs(30), Duration::from_secs(15), cc).unwrap();
    }

    #[test]
    fn is_private_network_detects_rfc1918_ipv4() {
        for ip in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
        ] {
            let addr: std::net::SocketAddr = format!("{ip}:1234").parse().unwrap();
            assert!(is_private_network(addr), "{ip} should be private");
        }
    }

    #[test]
    fn is_private_network_detects_loopback_and_link_local() {
        for ip in ["127.0.0.1", "169.254.1.1"] {
            let addr: std::net::SocketAddr = format!("{ip}:1234").parse().unwrap();
            assert!(is_private_network(addr), "{ip} should be private");
        }
    }

    #[test]
    fn is_private_network_rejects_public_ipv4() {
        for ip in ["8.8.8.8", "210.14.159.130", "101.200.139.81", "100.64.0.1"] {
            let addr: std::net::SocketAddr = format!("{ip}:1234").parse().unwrap();
            assert!(!is_private_network(addr), "{ip} should be public");
        }
    }

    #[test]
    fn is_private_network_detects_ipv6_private_ranges() {
        for ip in ["::1", "fd00::1", "fc00::1"] {
            let addr: std::net::SocketAddr = format!("[{ip}]:1234").parse().unwrap();
            assert!(is_private_network(addr), "{ip} should be private");
        }
    }

    #[test]
    fn is_private_network_rejects_public_ipv6() {
        let addr: std::net::SocketAddr = "[2001:4860:4860::8888]:1234".parse().unwrap();
        assert!(
            !is_private_network(addr),
            "public IPv6 should not be private"
        );
    }

    #[test]
    fn cc_resolve_auto_picks_bbr_for_private_target() {
        let addr: std::net::SocketAddr = "10.0.0.1:1234".parse().unwrap();
        assert_eq!(CcKind::Auto.resolve(Some(addr)), CcKind::Bbr);
    }

    #[test]
    fn cc_resolve_auto_picks_cubic_for_public_target() {
        let addr: std::net::SocketAddr = "8.8.8.8:1234".parse().unwrap();
        assert_eq!(CcKind::Auto.resolve(Some(addr)), CcKind::Cubic);
    }

    #[test]
    fn cc_resolve_auto_falls_back_to_bbr_without_target() {
        assert_eq!(CcKind::Auto.resolve(None), CcKind::Bbr);
    }

    #[test]
    fn cc_resolve_explicit_overrides_auto_detection() {
        let private: std::net::SocketAddr = "10.0.0.1:1234".parse().unwrap();
        let public: std::net::SocketAddr = "8.8.8.8:1234".parse().unwrap();
        assert_eq!(CcKind::Cubic.resolve(Some(private)), CcKind::Cubic);
        assert_eq!(CcKind::Bbr.resolve(Some(public)), CcKind::Bbr);
    }

    #[test]
    fn cc_from_str_parses_valid_values_case_insensitively() {
        for s in ["auto", "bbr", "cubic", "AUTO", "Bbr", "CUBIC"] {
            assert!(s.parse::<CcKind>().is_ok(), "{s} should parse");
        }
        assert_eq!("auto".parse::<CcKind>(), Ok(CcKind::Auto));
        assert_eq!("bbr".parse::<CcKind>(), Ok(CcKind::Bbr));
        assert_eq!("cubic".parse::<CcKind>(), Ok(CcKind::Cubic));
    }

    #[test]
    fn cc_from_str_rejects_invalid_values() {
        for s in ["", "reno", "vegas", "newreno", "auto,x"] {
            assert!(s.parse::<CcKind>().is_err(), "{s:?} should be rejected");
        }
    }

    #[test]
    fn cc_from_str_trims_surrounding_whitespace() {
        assert_eq!("  bbr".parse::<CcKind>(), Ok(CcKind::Bbr));
        assert_eq!("cubic\n".parse::<CcKind>(), Ok(CcKind::Cubic));
        assert_eq!(" auto ".parse::<CcKind>(), Ok(CcKind::Auto));
    }

    #[test]
    fn cc_from_env_parses_value_and_falls_back_to_auto() {
        const TEST_ENV: &str = "CLUM_CC_TEST_VAR";
        unsafe {
            std::env::set_var(TEST_ENV, "cubic");
        }
        assert_eq!(CcKind::from_env(TEST_ENV), CcKind::Cubic);
        unsafe {
            std::env::set_var(TEST_ENV, "bogus");
        }
        assert_eq!(CcKind::from_env(TEST_ENV), CcKind::Auto);
        unsafe {
            std::env::remove_var(TEST_ENV);
        }
        assert_eq!(CcKind::from_env(TEST_ENV), CcKind::Auto);
    }

    #[test]
    fn cc_factory_builds_controllers_for_concrete_kinds() {
        let _ = CcKind::Bbr.factory();
        let _ = CcKind::Cubic.factory();
        let _ = CcKind::Auto.factory();
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
