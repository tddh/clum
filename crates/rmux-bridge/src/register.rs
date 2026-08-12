use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use crate::bridge_audit::BridgeAuditDb;
use crate::cast_recorder;
use crate::interactive::InteractiveSession;
use crate::protocol::ProtocolProxy;
use clum_core::backoff::FullJitterBackoff;

pub struct RegisterConfig {
    pub server_addr: String,
    pub ca_cert: Option<String>,
    /// Shared with the control-stream reader so rotated tokens take effect
    /// on the next registration attempt without a restart.
    pub token: Arc<tokio::sync::RwLock<String>>,
    pub rmux_socket: String,
    pub recording_enabled: bool,
    pub recording_dir: std::path::PathBuf,
    pub recording_fsync_interval_secs: u64,
    pub idle_timeout_secs: u64,
    pub audit_db: Arc<BridgeAuditDb>,
}

pub async fn run_registration_loop(config: RegisterConfig) {
    let mut backoff = FullJitterBackoff::new(Duration::from_millis(500), Duration::from_secs(30));

    loop {
        match connect_and_register(&config).await {
            Ok(()) => {
                tracing::info!("registration session ended, reconnecting");
                backoff.reset();
                tokio::time::sleep(backoff.next_delay()).await;
            }
            Err(e) => {
                let delay = backoff.next_delay();
                tracing::warn!("registration failed: {e:#}, retrying in {delay:?}");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

async fn connect_and_register(config: &RegisterConfig) -> anyhow::Result<()> {
    let endpoint = clum_core::quic::client_endpoint(
        config.ca_cert.as_deref(),
        &[b"clum"],
        Duration::from_secs(120),
        clum_core::quic::DEFAULT_KEEPALIVE,
    )?;

    let server_addr = resolve_addr(&config.server_addr).await?;
    let server_name = extract_server_name(&config.server_addr);

    tracing::info!(addr = %server_addr, "connecting to server");
    let conn = endpoint
        .connect(server_addr, &server_name)?
        .await
        .context("QUIC handshake failed")?;

    tracing::info!("connected, sending registration");

    let (mut send, mut recv) = conn.open_bi().await?;

    let machine_id = read_machine_id();
    let os_info = read_os_info();
    let token = config.token.read().await.clone();

    let reg_msg = serde_json::json!({
        "type": "bridge_register",
        "token": token,
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": ["exec", "file", "forward", "interactive"],
        "machine_id": machine_id,
        "os_info": os_info,
    });
    write_frame(&mut send, &reg_msg).await?;

    let ack_data = read_frame(&mut recv).await?;
    let ack: serde_json::Value = serde_json::from_slice(&ack_data)?;

    if ack.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = ack
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        anyhow::bail!("registration rejected: {err}");
    }

    let hostname = ack
        .get("hostname")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    tracing::info!(hostname = %hostname, "registered, starting heartbeat + stream handler");

    let protocol_proxy = Arc::new(tokio::sync::RwLock::new(
        match ProtocolProxy::connect(&config.rmux_socket).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("rmux connect failed: {e}");
                return Err(e);
            }
        },
    ));

    let session_state: Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, InteractiveSession>>,
    > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let session_counts: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    // Heartbeat task: send pings on the control stream
    let heartbeat_conn = conn.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            let ping = serde_json::json!({"type": "ping"});
            if write_frame(&mut send, &ping).await.is_err() {
                break;
            }
        }
        heartbeat_conn.close(quinn::VarInt::from_u32(0), b"heartbeat ended");
    });

    // Control stream reader: handle server pushes (token rotation)
    let rotated_token = Arc::clone(&config.token);
    tokio::spawn(async move {
        while let Ok(data) = read_frame(&mut recv).await {
            let msg: serde_json::Value = match serde_json::from_slice(&data) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if msg.get("type").and_then(|v| v.as_str()) == Some("token_rotate") {
                if let Some(new_token) = msg.get("new_token").and_then(|v| v.as_str()) {
                    match persist_token(new_token).await {
                        Ok(()) => tracing::info!("token rotated and persisted"),
                        Err(e) => tracing::error!("failed to persist rotated token: {e}"),
                    }
                    // Update in-memory even if persist failed: the running
                    // process must use the new token on its next registration
                    // attempt; only a restart would fall back to the stale file.
                    *rotated_token.write().await = new_token.to_string();
                }
            }
        }
    });

    // Recording push loop: scan for new .cast files and push to Server
    let push_conn = conn.clone();
    let push_dir = config.recording_dir.clone();
    let push_enabled = config.recording_enabled;
    if push_enabled {
        tokio::spawn(async move {
            let mut pushed = load_pushed(&push_dir).await;
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                if push_conn.close_reason().is_some() {
                    break;
                }
                let files = match scan_cast_files(&push_dir).await {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                // Prune stale entries from pushed set (files that no longer exist)
                let stale: Vec<_> = pushed.iter().filter(|p| !p.exists()).cloned().collect();
                for p in &stale {
                    pushed.remove(p);
                }
                if !stale.is_empty() {
                    let _ = save_pushed(&push_dir, &pushed).await;
                }
                for path in files {
                    if pushed.contains(&path) {
                        continue;
                    }
                    match push_recording(&push_conn, &path).await {
                        Ok(()) => {
                            pushed.insert(path.clone());
                            let _ = save_pushed(&push_dir, &pushed).await;
                            tracing::info!(file = %path.display(), "recording pushed to server");
                            // Mark synced to prevent redundant Pull transfer
                            if let Some(date) = path
                                .parent()
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().to_string())
                            {
                                let fname = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                if let Err(e) =
                                    cast_recorder::mark_synced(&push_dir, &fname, &date).await
                                {
                                    tracing::debug!(
                                        file = %path.display(),
                                        "mark_synced after push failed: {e}"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!(file = %path.display(), "recording push failed: {e}");
                        }
                    }
                }
            }
        });
    }

    // Accept incoming streams from Server (tool execution requests)
    loop {
        match conn.accept_bi().await {
            Ok((stream_send, stream_recv)) => {
                let proxy = protocol_proxy.clone();
                let state = session_state.clone();
                let counts = session_counts.clone();
                let rec_dir = config.recording_dir.clone();
                let rec_enabled = config.recording_enabled;
                let rec_fsync = config.recording_fsync_interval_secs;
                let audit_db = config.audit_db.clone();
                let idle_timeout = config.idle_timeout_secs;
                tokio::spawn(async move {
                    if let Err(e) = crate::files::handle_quic_stream(
                        stream_send,
                        stream_recv,
                        proxy,
                        state,
                        counts,
                        rec_enabled,
                        rec_dir,
                        rec_fsync,
                        audit_db,
                        idle_timeout,
                    )
                    .await
                    {
                        tracing::debug!("server stream handler ended: {e}");
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => break,
            Err(quinn::ConnectionError::LocallyClosed) => break,
            Err(e) => {
                tracing::warn!("accept_bi error: {e}");
                break;
            }
        }
    }

    tracing::info!("connection closed, will reconnect");
    Ok(())
}

async fn resolve_addr(addr: &str) -> anyhow::Result<std::net::SocketAddr> {
    if let Ok(sock) = addr.parse::<std::net::SocketAddr>() {
        return Ok(sock);
    }
    let addrs: Vec<_> = tokio::net::lookup_host(addr).await?.collect();
    addrs.into_iter().next().context("DNS resolution failed")
}

fn extract_server_name(addr: &str) -> String {
    addr.split(':').next().unwrap_or("localhost").to_string()
}

fn read_machine_id() -> String {
    std::fs::read_to_string("/etc/machine-id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn read_os_info() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("PRETTY_NAME="))
                .map(|l| {
                    l.trim_start_matches("PRETTY_NAME=")
                        .trim_matches('"')
                        .to_string()
                })
        })
        .unwrap_or_else(|| format!("unknown {}", std::env::consts::ARCH))
}

async fn scan_cast_files(dir: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            let mut sub = tokio::fs::read_dir(&path).await?;
            while let Some(sub_entry) = sub.next_entry().await? {
                let sub_path = sub_entry.path();
                if sub_path.extension().map(|e| e == "cast").unwrap_or(false) {
                    files.push(sub_path);
                }
            }
        } else if path.extension().map(|e| e == "cast").unwrap_or(false) {
            files.push(path);
        }
    }
    Ok(files)
}

async fn push_recording(conn: &quinn::Connection, path: &std::path::Path) -> anyhow::Result<()> {
    let data = tokio::fs::read(path).await?;
    let filename = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown.cast".to_string());

    let (mut send, mut recv) = conn.open_bi().await?;

    let header = serde_json::json!({
        "type": "recording_push",
        "filename": filename,
        "size": data.len(),
    });
    write_frame(&mut send, &header).await?;

    send.write_all(&data).await?;
    send.finish()?;

    let _ack = read_frame(&mut recv).await?;
    Ok(())
}

async fn persist_token(token: &str) -> anyhow::Result<()> {
    let path = std::path::Path::new("/etc/clum/token");
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::write(path, token).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(())
}

const PUSHED_STATE_FILE: &str = ".pushed.json";

async fn load_pushed(dir: &Path) -> HashSet<PathBuf> {
    let path = dir.join(PUSHED_STATE_FILE);
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => {
            let paths: Vec<String> = serde_json::from_str(&content).unwrap_or_default();
            paths.into_iter().map(PathBuf::from).collect()
        }
        Err(_) => HashSet::new(),
    }
}

async fn save_pushed(dir: &Path, pushed: &HashSet<PathBuf>) -> anyhow::Result<()> {
    let path = dir.join(PUSHED_STATE_FILE);
    let entries: Vec<String> = pushed
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let json = serde_json::to_string(&entries)?;
    tokio::fs::write(&path, json).await?;
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
