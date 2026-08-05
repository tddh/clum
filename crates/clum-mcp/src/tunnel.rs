use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use quinn::Connection;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use clum_core::types::HostConfig;

use crate::transport::connect_to_bridge_quic_tunnel;

const STREAM_TUNNEL: u8 = 0x05;
const TUNNEL_BUFFER_SIZE: usize = 65536;
const MAX_HOST_LEN: usize = 253;

fn check_tunnel_target(host: &HostConfig, remote_host: &str, remote_port: u16) -> Result<()> {
    let targets = match &host.allowed_tunnel_targets {
        Some(t) => t,
        None => return Ok(()),
    };

    let target = format!("{}:{}", remote_host, remote_port);
    let matched = targets.iter().any(|pattern| {
        glob::Pattern::new(pattern)
            .map(|p| p.matches(&target))
            .unwrap_or(false)
    });

    if matched {
        Ok(())
    } else {
        anyhow::bail!(
            "tunnel target {}:{} not in allowed list for host '{}' (allowed: {:?})",
            remote_host,
            remote_port,
            host.name,
            targets
        )
    }
}

#[derive(Debug, Serialize)]
pub struct TunnelInfo {
    pub tunnel_id: String,
    pub local_addr: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub created_at: DateTime<Utc>,
    pub active_connections: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

struct Tunnel {
    pub id: String,
    pub local_addr: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub created_at: DateTime<Utc>,
    pub active_connections: Arc<AtomicUsize>,
    pub listener_task: JoinHandle<()>,
    pub group: Option<String>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.listener_task.abort();
    }
}

pub struct TunnelManager {
    tunnels: Arc<Mutex<HashMap<String, Tunnel>>>,
}

impl TunnelManager {
    pub fn new() -> Self {
        Self {
            tunnels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        host: &HostConfig,
        local_addr: &str,
        local_port: u16,
        remote_host: String,
        remote_port: u16,
        ca_cert_path: Option<&str>,
        registry: &std::sync::Arc<crate::registry::BridgeRegistry>,
        group: Option<String>,
    ) -> Result<TunnelInfo> {
        if remote_host.len() > MAX_HOST_LEN {
            anyhow::bail!(
                "remote host too long: {} (max {})",
                remote_host.len(),
                MAX_HOST_LEN
            );
        }

        check_tunnel_target(host, &remote_host, remote_port)?;

        let bind_addr = format!("{}:{}", local_addr, local_port);

        let listener = TcpListener::bind(&bind_addr)
            .await
            .with_context(|| format!("failed to bind to {}", bind_addr))?;

        let conn = if let Some(bridge) = registry.get(&host.name).await {
            bridge.conn.clone()
        } else {
            let addr = host
                .bridge_addr
                .as_deref()
                .with_context(|| format!("host '{}': bridge_addr not configured", host.name))?;
            let token = host
                .bridge_token
                .as_deref()
                .with_context(|| format!("host '{}': bridge_token not configured", host.name))?;
            let (conn, auth_send, auth_recv) =
                connect_to_bridge_quic_tunnel(addr, token, ca_cert_path)
                    .await
                    .with_context(|| "failed to connect to bridge")?;
            tokio::spawn(async move {
                let mut auth_send = auth_send;
                let mut auth_recv = auth_recv;
                auth_send.finish().ok();
                let _ = auth_recv.read_to_end(0).await;
            });
            conn
        };

        let tunnel_id = format!("t_{}", Uuid::new_v4());
        let active_connections = Arc::new(AtomicUsize::new(0));
        let created_at = Utc::now();

        let tunnels = self.tunnels.clone();
        let tunnel_id_clone = tunnel_id.clone();
        let conn_clone = conn.clone();
        let remote_host_clone = remote_host.clone();
        let active_conn_clone = active_connections.clone();

        let listener_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((tcp_stream, peer_addr)) => {
                        tracing::info!(
                            "tunnel {} accepted connection from {}",
                            tunnel_id_clone,
                            peer_addr
                        );

                        let conn = conn_clone.clone();
                        let remote_host = remote_host_clone.clone();
                        let remote_port = remote_port;
                        let active = active_conn_clone.clone();
                        let tunnel_id_inner = tunnel_id_clone.clone();

                        active.fetch_add(1, Ordering::Relaxed);

                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_tunnel_connection(tcp_stream, conn, remote_host, remote_port)
                                    .await
                            {
                                tracing::warn!(
                                    "tunnel {} connection error: {}",
                                    tunnel_id_inner,
                                    e
                                );
                            }
                            active.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Err(e) => {
                        tracing::warn!("tunnel {} accept error: {}", tunnel_id_clone, e);
                        if e.kind() == std::io::ErrorKind::InvalidInput {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }

            tunnels.lock().await.remove(&tunnel_id_clone);
        });

        let info = TunnelInfo {
            tunnel_id: tunnel_id.clone(),
            local_addr: bind_addr,
            local_port,
            remote_host: remote_host.clone(),
            remote_port,
            created_at,
            active_connections: 0,
            group: group.clone(),
        };

        let tunnel = Tunnel {
            id: tunnel_id,
            local_addr: info.local_addr.clone(),
            local_port,
            remote_host,
            remote_port,
            created_at,
            active_connections,
            listener_task,
            group,
        };

        self.tunnels
            .lock()
            .await
            .insert(info.tunnel_id.clone(), tunnel);

        Ok(info)
    }

    pub async fn list(&self) -> Vec<TunnelInfo> {
        let tunnels = self.tunnels.lock().await;
        tunnels
            .values()
            .map(|t| TunnelInfo {
                tunnel_id: t.id.clone(),
                local_addr: t.local_addr.clone(),
                local_port: t.local_port,
                remote_host: t.remote_host.clone(),
                remote_port: t.remote_port,
                created_at: t.created_at,
                active_connections: t.active_connections.load(Ordering::Relaxed),
                group: t.group.clone(),
            })
            .collect()
    }

    pub async fn close(&self, tunnel_id: &str) -> Result<()> {
        let mut tunnels = self.tunnels.lock().await;
        if let Some(tunnel) = tunnels.remove(tunnel_id) {
            tunnel.listener_task.abort();
            Ok(())
        } else {
            anyhow::bail!("tunnel not found: {}", tunnel_id)
        }
    }
}

async fn handle_tunnel_connection(
    tcp_stream: TcpStream,
    conn: Connection,
    remote_host: String,
    remote_port: u16,
) -> Result<()> {
    let (mut quic_send, mut quic_recv) = conn
        .open_bi()
        .await
        .with_context(|| "failed to open QUIC stream")?;

    quic_send
        .write_all(&[STREAM_TUNNEL])
        .await
        .with_context(|| "failed to write stream type")?;

    let host_bytes = remote_host.as_bytes();
    quic_send
        .write_all(&(host_bytes.len() as u16).to_le_bytes())
        .await
        .with_context(|| "failed to write host length")?;

    quic_send
        .write_all(host_bytes)
        .await
        .with_context(|| "failed to write host")?;

    quic_send
        .write_all(&remote_port.to_le_bytes())
        .await
        .with_context(|| "failed to write port")?;

    let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();

    let tcp_to_quic = async {
        let mut buf = vec![0u8; TUNNEL_BUFFER_SIZE];
        loop {
            let n = tcp_read.read(&mut buf).await?;
            if n == 0 {
                quic_send.finish()?;
                break;
            }
            quic_send.write_all(&buf[..n]).await?;
        }
        Ok::<_, anyhow::Error>(())
    };

    let quic_to_tcp = async {
        let mut buf = vec![0u8; TUNNEL_BUFFER_SIZE];
        loop {
            match quic_recv.read(&mut buf).await? {
                Some(0) | None => {
                    let _ = tcp_write.shutdown().await;
                    break;
                }
                Some(n) => tcp_write.write_all(&buf[..n]).await?,
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    tokio::try_join!(tcp_to_quic, quic_to_tcp)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_host(targets: Option<Vec<String>>) -> HostConfig {
        HostConfig {
            name: "test-host".to_string(),
            bridge_addr: Some("10.0.0.1:9778".to_string()),
            bridge_token: Some("tok".to_string()),
            group: "test".to_string(),
            tags: vec![],
            labels: HashMap::new(),
            allowed_tunnel_targets: targets,
        }
    }

    #[test]
    fn test_no_targets_allows_all() {
        let host = make_host(None);
        assert!(check_tunnel_target(&host, "127.0.0.1", 22).is_ok());
        assert!(check_tunnel_target(&host, "10.0.0.1", 3306).is_ok());
    }

    #[test]
    fn test_exact_match() {
        let host = make_host(Some(vec!["127.0.0.1:5432".to_string()]));
        assert!(check_tunnel_target(&host, "127.0.0.1", 5432).is_ok());
        assert!(check_tunnel_target(&host, "127.0.0.1", 3306).is_err());
    }

    #[test]
    fn test_glob_match() {
        let host = make_host(Some(vec!["10.0.1.*:*".to_string()]));
        assert!(check_tunnel_target(&host, "10.0.1.20", 5432).is_ok());
        assert!(check_tunnel_target(&host, "10.0.1.100", 80).is_ok());
        assert!(check_tunnel_target(&host, "10.0.2.1", 80).is_err());
    }

    #[test]
    fn test_port_glob() {
        let host = make_host(Some(vec!["*:3306".to_string()]));
        assert!(check_tunnel_target(&host, "10.0.1.20", 3306).is_ok());
        assert!(check_tunnel_target(&host, "127.0.0.1", 3306).is_ok());
        assert!(check_tunnel_target(&host, "127.0.0.1", 5432).is_err());
    }
}
