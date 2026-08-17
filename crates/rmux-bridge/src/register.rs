use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::watch;

use crate::bridge_audit::BridgeAuditDb;
use crate::cast_recorder;
use crate::interactive::SessionTracker;
use crate::protocol::ProtocolProxy;
use clum_core::backoff::FullJitterBackoff;
use clum_core::quic::{read_frame, write_frame};

/// 优雅关闭信号：SIGTERM 触发一次，供注册循环与连接处理任务等待。
///
/// 内部用 `watch` 保证竞态安全：`trigger` 后值持久为 `true`，
/// 后续 `wait` 立即返回，不存在丢失唤醒。
pub struct Shutdown {
    sender: watch::Sender<bool>,
    // 持有 receiver 保持 channel 打开；否则全部 receiver drop 后
    // channel 关闭，trigger 的 send 会失败、值无法更新。
    _receiver: watch::Receiver<bool>,
}

impl Shutdown {
    pub fn new() -> Arc<Self> {
        let (sender, receiver) = watch::channel(false);
        Arc::new(Self {
            sender,
            _receiver: receiver,
        })
    }

    /// 触发关闭：置位并唤醒所有等待者。多次调用幂等。
    pub fn trigger(&self) {
        let _ = self.sender.send(true);
    }

    /// 是否已触发（同步查询，供循环条件使用）。
    pub fn is_triggered(&self) -> bool {
        *self.sender.borrow()
    }

    /// 异步等待触发。若已触发则立即返回。
    pub async fn wait(&self) {
        let mut rx = self.sender.subscribe();
        if *rx.borrow() {
            return;
        }
        let _ = rx.changed().await;
    }
}

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
    /// SIGTERM 优雅关闭信号。
    pub shutdown: Arc<Shutdown>,
}

pub async fn run_registration_loop(config: RegisterConfig) {
    let mut backoff = FullJitterBackoff::new(Duration::from_millis(500), Duration::from_secs(30));

    while !config.shutdown.is_triggered() {
        match connect_and_register(&config).await {
            Ok(()) => {
                tracing::info!("registration session ended, reconnecting");
                backoff.reset();
                tokio::select! {
                    _ = config.shutdown.wait() => break,
                    _ = tokio::time::sleep(backoff.next_delay()) => {}
                }
            }
            Err(e) => {
                if config.shutdown.is_triggered() {
                    break;
                }
                let delay = backoff.next_delay();
                tracing::warn!("registration failed: {e:#}, retrying in {delay:?}");
                tokio::select! {
                    _ = config.shutdown.wait() => break,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }

    tracing::info!("registration loop exited");
}

async fn connect_and_register(config: &RegisterConfig) -> anyhow::Result<()> {
    let server_addr = resolve_addr(&config.server_addr).await?;
    let cc = clum_core::quic::CcKind::from_env("BRIDGE_CC").resolve(Some(server_addr));
    let endpoint = clum_core::quic::client_endpoint(
        config.ca_cert.as_deref(),
        &[b"clum"],
        Duration::from_secs(120),
        clum_core::quic::DEFAULT_KEEPALIVE,
        cc,
    )?;

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

    let sessions = SessionTracker::new();

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
                    if let Err(e) = save_pushed(&push_dir, &pushed).await {
                        tracing::warn!("failed to save pushed state: {e}");
                    }
                }
                for path in files {
                    if pushed.contains(&path) {
                        continue;
                    }
                    match push_recording(&push_conn, &path).await {
                        Ok(()) => {
                            pushed.insert(path.clone());
                            if let Err(e) = save_pushed(&push_dir, &pushed).await {
                                tracing::warn!("failed to save pushed state: {e}");
                            }
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
        tokio::select! {
            _ = config.shutdown.wait() => {
                conn.close(quinn::VarInt::from_u32(0), b"shutdown");
                // close() 只设置关闭标志，CONNECTION_CLOSE 帧由 endpoint 的 driver
                // task 在下一次 drive 时构造并写入 UDP。等待一小段让帧 flush，
                // 否则进程立即退出会丢弃帧，server 只能等 idle timeout 才 unregister。
                tokio::time::sleep(Duration::from_millis(500)).await;
                tracing::info!("graceful shutdown: sent CONNECTION_CLOSE to server");
                return Ok(());
            }
            r = conn.accept_bi() => {
                match r {
                    Ok((stream_send, stream_recv)) => {
                        let proxy = protocol_proxy.clone();
                        let state = sessions.state.clone();
                        let counts = sessions.counts.clone();
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
        }
    }

    tracing::info!(reason = ?conn.close_reason(), "connection closed, will reconnect");
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
                if sub_path.extension().map(|e| e == "cast").unwrap_or(false)
                    && is_unsynced_completed(&sub_path).await
                {
                    files.push(sub_path);
                }
            }
        } else if path.extension().map(|e| e == "cast").unwrap_or(false)
            && is_unsynced_completed(&path).await
        {
            files.push(path);
        }
    }
    Ok(files)
}

/// 只推送「已完成且未同步」的录制：`.meta` 只在 finalize 时生成，
/// `synced: false` 表示尚未被 push 或 pull 拿走。正在录制的文件没有
/// `.meta`，永远不会被扫到——避免把半成品推给 server。
async fn is_unsynced_completed(cast_path: &std::path::Path) -> bool {
    let meta_path = cast_path.with_extension("meta");
    match tokio::fs::read_to_string(&meta_path).await {
        Ok(content) => {
            serde_json::from_str::<serde_json::Value>(&content)
                .ok()
                .and_then(|m| m["synced"].as_bool())
                == Some(false)
        }
        Err(_) => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_wait_returns_after_trigger() {
        let s = Shutdown::new();
        assert!(!s.is_triggered());

        let s2 = s.clone();
        let handle = tokio::spawn(async move { s2.wait().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        s.trigger();

        handle.await.unwrap();
        assert!(s.is_triggered());
    }

    #[tokio::test]
    async fn shutdown_wait_returns_immediately_if_already_triggered() {
        let s = Shutdown::new();
        s.trigger();
        tokio::time::timeout(Duration::from_secs(1), s.wait())
            .await
            .expect("wait should return immediately after trigger");
        assert!(s.is_triggered());
    }

    #[tokio::test]
    async fn shutdown_trigger_is_idempotent() {
        let s = Shutdown::new();
        s.trigger();
        s.trigger();
        assert!(s.is_triggered());
    }

    #[tokio::test]
    async fn scan_cast_files_only_returns_unsynced_completed() {
        let dir = tempfile::tempdir().unwrap();
        let date_dir = dir.path().join("2026-08-14");
        tokio::fs::create_dir_all(&date_dir).await.unwrap();

        // 已完成且未同步 → 应被返回
        let done = date_dir.join("done.cast");
        tokio::fs::write(&done, "[1, \"o\", \"x\"]\n")
            .await
            .unwrap();
        tokio::fs::write(
            date_dir.join("done.meta"),
            r#"{"synced": false, "sha256": "abc"}"#,
        )
        .await
        .unwrap();

        // 已完成且已同步 → 不应被返回
        let synced = date_dir.join("synced.cast");
        tokio::fs::write(&synced, "[1, \"o\", \"y\"]\n")
            .await
            .unwrap();
        tokio::fs::write(
            date_dir.join("synced.meta"),
            r#"{"synced": true, "sha256": "def"}"#,
        )
        .await
        .unwrap();

        // 正在录制（无 .meta）→ 不应被返回
        let in_progress = date_dir.join("in_progress.cast");
        tokio::fs::write(&in_progress, "[0.5, \"o\", \"z\"]\n")
            .await
            .unwrap();

        let files = scan_cast_files(dir.path()).await.unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert_eq!(
            names,
            vec!["done.cast"],
            "only unsynced completed should be pushed"
        );
    }
}
