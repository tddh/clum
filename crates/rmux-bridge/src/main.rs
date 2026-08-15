mod auth;
mod bridge_audit;
mod cast_recorder;
mod config;
mod files;
mod interactive;
mod protocol;
mod proxy;
mod register;
mod terminal_state;
mod tls;

use anyhow::Context;
use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::interactive::SessionTracker;
use crate::protocol::ProtocolProxy;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = config::BridgeConfig::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.log_level)),
        )
        .init();

    // quinn needs explicit crypto provider in musl builds
    let _ = rustls::crypto::ring::default_provider().install_default();

    let audit_db = Arc::new(
        bridge_audit::BridgeAuditDb::open(&config.resolve_audit_db_path())
            .expect("failed to open bridge audit db"),
    );

    // 直连模式（未配置 server_addr）下才启动本地 QUIC 监听器。
    // 注册模式（Central Server）下所有请求都走注册连接，无需监听本地端口，
    // 也无需加载 bridge 自身的 TLS 服务端证书（certs/bridge.crt）。
    if config.server_addr.is_none() {
        tracing::info!("rmux-bridge starting on {}", config.quic_listen_addr);

        let conn_limit = if config.max_connections > 0 {
            Some(Arc::new(Semaphore::new(config.max_connections)))
        } else {
            None
        };

        // ─── QUIC file transfer listener ───
        let quic_config = config.clone();
        let quic_conn_limit_pre = conn_limit.clone();
        let recording_enabled = config.recording_enabled;
        let recording_dir = config.resolve_recording_dir();
        let fsync_interval_secs = config.recording_fsync_interval_secs;
        let idle_timeout_secs = config.idle_timeout_secs;
        let quic_audit_db = audit_db.clone();
        tokio::spawn(async move {
            let conn_limit = quic_conn_limit_pre;
            let tls_cfg =
                match tls::load_quic_server_config(&quic_config.tls_cert, &quic_config.tls_key) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("failed to load QUIC TLS config: {}", e);
                        return;
                    }
                };
            let quic_addr: SocketAddr = match quic_config.quic_listen_addr.parse() {
                Ok(a) => a,
                Err(e) => {
                    tracing::error!("invalid QUIC listen addr: {}", e);
                    return;
                }
            };
            let endpoint = match (|| -> anyhow::Result<quinn::Endpoint> {
                let socket = clum_core::quic::build_udp_socket(quic_addr)?;
                let runtime = quinn::default_runtime().context("no async runtime found")?;
                Ok(quinn::Endpoint::new(
                    quinn::EndpointConfig::default(),
                    Some(tls_cfg),
                    socket,
                    runtime,
                )?)
            })() {
                Ok(ep) => ep,
                Err(e) => {
                    tracing::error!("failed to create QUIC endpoint: {e:#}");
                    return;
                }
            };
            tracing::info!("QUIC file transfer listening on {}", quic_addr);

            let auth_token = std::sync::Arc::new(quic_config.auth_token.clone());
            let quic_rmux_socket = Arc::new(quic_config.rmux_socket.clone());
            let quic_conn_limit = conn_limit.clone();

            while let Some(incoming) = endpoint.accept().await {
                let _permit = if let Some(ref lim) = quic_conn_limit {
                    match lim.clone().acquire_owned().await {
                        Ok(p) => Some(p),
                        Err(_) => break,
                    }
                } else {
                    None
                };

                let token = auth_token.clone();
                let rmux_socket = quic_rmux_socket.clone();
                let conn_recording_dir = recording_dir.clone();
                let conn_audit_db = quic_audit_db.clone();
                tokio::spawn(async move {
                    let _permit = _permit;
                    let conn = match incoming.await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("QUIC connection failed: {}", e);
                            return;
                        }
                    };

                    let (mut auth_send, mut auth_recv) = match conn.accept_bi().await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("QUIC accept_bi failed: {}", e);
                            return;
                        }
                    };

                    let client_addr = conn.remote_address().to_string();

                    if let Err(e) = auth::authenticate_quic(
                        &mut auth_send,
                        &mut auth_recv,
                        &token,
                        conn_audit_db.clone(),
                        client_addr,
                    )
                    .await
                    {
                        tracing::warn!("QUIC auth failed: {}", e);
                        return;
                    }

                    let protocol_proxy = Arc::new(tokio::sync::RwLock::new(
                        match ProtocolProxy::connect(&rmux_socket).await {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::error!("QUIC rmux connect failed: {}", e);
                                return;
                            }
                        },
                    ));

                    let sessions = SessionTracker::new();

                    loop {
                        match conn.accept_bi().await {
                            Ok((send, recv)) => {
                                let proxy = protocol_proxy.clone();
                                let state = sessions.state.clone();
                                let counts = sessions.counts.clone();
                                let rec_dir = conn_recording_dir.clone();
                                let rec_enabled = recording_enabled;
                                let rec_fsync = fsync_interval_secs;
                                let stream_audit_db = conn_audit_db.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = files::handle_quic_stream(
                                        send,
                                        recv,
                                        proxy,
                                        state,
                                        counts,
                                        rec_enabled,
                                        rec_dir,
                                        rec_fsync,
                                        stream_audit_db,
                                        idle_timeout_secs,
                                    )
                                    .await
                                    {
                                        tracing::warn!("QUIC stream error: {}", e);
                                    }
                                });
                            }
                            Err(quinn::ConnectionError::ApplicationClosed { .. }) => break,
                            Err(quinn::ConnectionError::LocallyClosed) => break,
                            Err(e) => {
                                tracing::warn!("QUIC accept_bi error: {}", e);
                                break;
                            }
                        }
                    }
                });
            }
        });
        // ─── end QUIC listener ───
    }

    // ─── Periodic recording cleanup ───
    if config.recording_enabled {
        let cleanup_dir = config.resolve_recording_dir();
        let retention_days = config.recording_retention_days;
        let max_size_mb = config.recording_max_size_mb;
        let cleanup_audit_db = audit_db.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                match cast_recorder::cleanup_recordings(&cleanup_dir, retention_days, max_size_mb)
                    .await
                {
                    Ok((deleted, freed)) if deleted > 0 => {
                        tracing::info!(
                            files_deleted = deleted,
                            bytes_freed = freed,
                            "recording cleanup completed"
                        );
                        cleanup_audit_db
                            .log(bridge_audit::BridgeEvent {
                                event_type: "recording_cleanup".to_string(),
                                client_addr: String::new(),
                                client_id: None,
                                session_name: None,
                                pane_id: None,
                                cols: None,
                                rows: None,
                                detail: Some(serde_json::json!({
                                    "files_deleted": deleted,
                                    "bytes_freed": freed
                                })),
                                duration_secs: None,
                                exit_code: None,
                            })
                            .await;
                    }
                    Err(e) => {
                        tracing::error!("recording cleanup failed: {e}");
                    }
                    _ => {}
                }

                if let Err(e) = cleanup_audit_db.cleanup(retention_days, max_size_mb).await {
                    tracing::error!("bridge audit cleanup failed: {e}");
                }
            }
        });
    }
    // ─── end recording cleanup ───

    // ─── Central server registration ───
    if let Some(server_addr) = &config.server_addr {
        let shutdown = register::Shutdown::new();

        // SIGTERM 监听：触发优雅关闭，主动断开到 server 的 QUIC 连接，
        // 让 server 立即 unregister，新进程重启后无需等待 idle timeout。
        {
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("failed to install SIGTERM handler");
                sigterm.recv().await;
                tracing::info!("SIGTERM received, triggering graceful shutdown");
                shutdown.trigger();
            });
        }

        let token = load_registration_token(&config.auth_token);
        let reg_config = register::RegisterConfig {
            server_addr: server_addr.clone(),
            ca_cert: config
                .ca_cert
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            token: std::sync::Arc::new(tokio::sync::RwLock::new(token)),
            rmux_socket: config.rmux_socket.clone(),
            recording_enabled: config.recording_enabled,
            recording_dir: config.resolve_recording_dir(),
            recording_fsync_interval_secs: config.recording_fsync_interval_secs,
            idle_timeout_secs: config.idle_timeout_secs,
            audit_db: audit_db.clone(),
            shutdown: shutdown.clone(),
        };
        let reg_handle = tokio::spawn(register::run_registration_loop(reg_config));

        shutdown.wait().await;
        // 等注册循环完成优雅关闭（close + flush CONNECTION_CLOSE 帧）。
        // main 若立即返回会打断循环里的 sleep，导致 close 帧发不出去，
        // server 只能等 idle timeout 才 unregister。
        if let Err(e) = reg_handle.await {
            tracing::error!("registration loop failed: {e}");
        }
        tracing::info!("bridge exited");
        return Ok(());
    }
    // ─── end registration ───

    // Block forever — QUIC listener runs in background task
    std::future::pending::<anyhow::Result<()>>().await
}

fn load_registration_token(env_token: &str) -> String {
    if let Ok(file_token) = std::fs::read_to_string("/etc/clum/token") {
        let trimmed = file_token.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    env_token.to_string()
}
