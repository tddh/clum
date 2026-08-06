//! Periodic recording sync: pull unsynced `.cast` files from bridges to local
//! storage, plus helpers to query the locally synced recording tree.
//!
//! The sync loop periodically asks every bridge for its list of unsynced
//! recordings (JSON command `list_unsynced_recordings` on a 0x01 stream),
//! downloads each file via the 0x03 file-download stream, verifies the sha256,
//! writes it to `{recordings_dir}/{host}/{date}/{file}`, then tells the bridge
//! to mark the file synced (JSON command `mark_synced`). After all hosts have
//! been processed, locally stored recordings older than the retention window
//! (and in excess of the size cap) are pruned.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use clum_core::HostConfig;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::registry::BridgeRegistry;
use crate::router::HostRouter;
use crate::transport::{
    connect_to_bridge_hybrid_stream, connect_to_bridge_quic, recv_json_frame, send_json_frame,
    BridgeStream,
};

/// Maximum size of a single recording file we are willing to buffer in memory
/// for sha256 verification (256 MB).
const MAX_RECORDING_SIZE: u64 = 256 * 1024 * 1024;

/// Configuration for the periodic recording sync loop.
pub struct RecordingSyncConfig {
    /// How often to run a full sync pass, in seconds.
    pub interval_secs: u64,
    /// Local root directory recordings are synced into.
    pub recordings_dir: PathBuf,
    /// Delete local recordings older than this many days.
    pub retention_days: u32,
    /// Best-effort cap on total local recording size, in MB.
    pub max_size_mb: u64,
}

/// Resolve the default recordings directory: `~/.clum/recordings`.
pub fn default_recordings_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".clum/recordings")
}

/// Run the recording sync loop forever. Intended to be spawned as a background
/// task. Individual sync failures are logged and never terminate the loop.
pub async fn run_sync_loop(
    config: RecordingSyncConfig,
    router: Arc<HostRouter>,
    registry: Arc<BridgeRegistry>,
    ca_cert_path: Option<String>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(config.interval_secs));
    // The first tick fires immediately; we still want a pass right away so that
    // freshly started servers begin syncing without waiting a full interval.
    loop {
        interval.tick().await;
        if let Err(e) = sync_all_hosts(&config, &router, &registry, ca_cert_path.as_deref()).await {
            tracing::error!("recording sync failed: {e}");
        }
    }
}

async fn sync_all_hosts(
    config: &RecordingSyncConfig,
    router: &HostRouter,
    registry: &BridgeRegistry,
    ca_cert_path: Option<&str>,
) -> anyhow::Result<()> {
    let hosts = router.list();

    for host in hosts {
        match sync_host(config, &host, registry, ca_cert_path).await {
            Ok(count) if count > 0 => {
                tracing::info!(host = %host.name, files = count, "recordings synced");
            }
            Err(e) => {
                tracing::warn!(host = %host.name, "recording sync failed: {e}");
            }
            _ => {}
        }
    }

    cleanup_local_recordings(
        &config.recordings_dir,
        config.retention_days,
        config.max_size_mb,
    )
    .await?;

    Ok(())
}

fn resolve_bridge_addr_token(host: &HostConfig) -> anyhow::Result<(&str, &str)> {
    let addr = host
        .bridge_addr
        .as_deref()
        .with_context(|| format!("host '{}': bridge_addr not configured", host.name))?;
    let token = host
        .bridge_token
        .as_deref()
        .with_context(|| format!("host '{}': bridge_token not configured", host.name))?;
    Ok((addr, token))
}

async fn open_registry_json_stream(
    registry: &BridgeRegistry,
    host_name: &str,
) -> Option<BridgeStream> {
    let bridge = registry.get(host_name).await?;
    if bridge.conn.close_reason().is_some() {
        return None;
    }
    let (mut send, recv) = bridge.conn.open_bi().await.ok()?;
    tokio::io::AsyncWriteExt::write_all(&mut send, &[0x01])
        .await
        .ok()?;
    Some(BridgeStream::Quic {
        conn: bridge.conn.clone(),
        send,
        recv,
    })
}

async fn sync_host(
    config: &RecordingSyncConfig,
    host: &HostConfig,
    registry: &BridgeRegistry,
    ca_cert_path: Option<&str>,
) -> anyhow::Result<usize> {
    let mut stream = match open_registry_json_stream(registry, &host.name).await {
        Some(s) => s,
        None => {
            let (addr, token) = resolve_bridge_addr_token(host)?;
            connect_to_bridge_hybrid_stream(addr, token, ca_cert_path, 3, 30, 10).await?
        }
    };

    send_json_frame(&mut stream, &json!({"command": "list_unsynced_recordings"})).await?;
    let resp = recv_json_frame(&mut stream).await?;

    if let Some(err) = resp.get("error").and_then(|v| v.as_str()) {
        anyhow::bail!("bridge returned error: {err}");
    }

    let files = resp
        .get("files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut synced = 0;
    for file_info in &files {
        let file_name = file_info["file"].as_str().unwrap_or("");
        let date = file_info["date"].as_str().unwrap_or("");
        let expected_sha = file_info["sha256"].as_str().unwrap_or("");

        if file_name.is_empty() || date.is_empty() {
            continue;
        }
        if !is_date_dir(date) || file_name.contains('/') || file_name.contains("..") {
            tracing::warn!(
                file = file_name,
                date = date,
                "skipping unsafe recording entry"
            );
            continue;
        }

        let remote_path = file_info["path"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("recordings/{}/{}", date, file_name));
        match download_recording(host, registry, ca_cert_path, &remote_path).await {
            Ok(data) => {
                let mut hasher = Sha256::new();
                hasher.update(&data);
                let actual_sha = hex::encode(hasher.finalize());

                if !expected_sha.is_empty() && actual_sha != expected_sha {
                    tracing::warn!(
                        file = file_name,
                        expected = expected_sha,
                        actual = %actual_sha,
                        "sha256 mismatch, skipping"
                    );
                    continue;
                }

                let local_dir = config.recordings_dir.join(&host.name).join(date);
                tokio::fs::create_dir_all(&local_dir).await?;
                let local_path = local_dir.join(file_name);
                tokio::fs::write(&local_path, &data).await?;

                mark_synced_on_bridge(host, registry, ca_cert_path, file_name, date).await?;
                synced += 1;
            }
            Err(e) => {
                tracing::warn!(file = file_name, "download failed: {e}");
            }
        }
    }

    Ok(synced)
}

/// Download a single recording file via the 0x03 file-download stream and
/// return its bytes. Mirrors the wire protocol used in `crate::files`.
async fn download_recording(
    host: &HostConfig,
    registry: &BridgeRegistry,
    ca_cert_path: Option<&str>,
    remote_path: &str,
) -> anyhow::Result<Vec<u8>> {
    let (mut send, mut recv) = match registry.get(&host.name).await {
        Some(bridge) if bridge.conn.close_reason().is_none() => bridge
            .conn
            .open_bi()
            .await
            .context("open_bi on registered conn for download")?,
        _ => {
            let (addr, token) = resolve_bridge_addr_token(host)?;
            let (conn, _auth_send, _auth_recv) =
                connect_to_bridge_quic(addr, token, ca_cert_path).await?;
            conn.open_bi().await?
        }
    };

    // Request: [1B stream_type=0x03][2B LE path_len][path_bytes]
    send.write_all(&[0x03]).await?;
    send.write_all(&(remote_path.len() as u16).to_le_bytes())
        .await?;
    send.write_all(remote_path.as_bytes()).await?;
    send.finish()?;

    // Response: [1B status][8B LE file_size][32B sha256][file_bytes]
    let mut status = [0u8; 1];
    recv.read_exact(&mut status).await?;
    match status[0] {
        0x00 => {}
        0x02 => {
            let mut msg_len = [0u8; 2];
            recv.read_exact(&mut msg_len).await?;
            let len = u16::from_le_bytes(msg_len) as usize;
            let mut msg = vec![0u8; len];
            recv.read_exact(&mut msg).await?;
            anyhow::bail!("download failed: {}", String::from_utf8_lossy(&msg));
        }
        code => anyhow::bail!("download failed: unexpected status 0x{code:02x}"),
    }

    let mut size_buf = [0u8; 8];
    recv.read_exact(&mut size_buf).await?;
    let file_size = u64::from_le_bytes(size_buf);
    if file_size > MAX_RECORDING_SIZE {
        anyhow::bail!(
            "recording too large: {} bytes (max {})",
            file_size,
            MAX_RECORDING_SIZE
        );
    }

    let mut sha_buf = [0u8; 32];
    recv.read_exact(&mut sha_buf).await?;

    let mut data = vec![0u8; file_size as usize];
    recv.read_exact(&mut data).await?;

    Ok(data)
}

async fn mark_synced_on_bridge(
    host: &HostConfig,
    registry: &BridgeRegistry,
    ca_cert_path: Option<&str>,
    file_name: &str,
    date: &str,
) -> anyhow::Result<()> {
    let mut stream = match open_registry_json_stream(registry, &host.name).await {
        Some(s) => s,
        None => {
            let (addr, token) = resolve_bridge_addr_token(host)?;
            connect_to_bridge_hybrid_stream(addr, token, ca_cert_path, 3, 30, 10).await?
        }
    };

    let cmd = json!({
        "command": "mark_synced",
        "params": {"file": file_name, "date": date},
    });
    send_json_frame(&mut stream, &cmd).await?;
    let resp = recv_json_frame(&mut stream).await?;
    if resp.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        let err = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("mark_synced failed: {err}");
    }
    Ok(())
}

/// Delete local recordings older than `retention_days`, then enforce the
/// `max_size_mb` cap by removing the oldest date directories first.
///
/// Layout: `{recordings_dir}/{host}/{date}/...` where `date` is YYYY-MM-DD.
async fn cleanup_local_recordings(
    recordings_dir: &Path,
    retention_days: u32,
    max_size_mb: u64,
) -> anyhow::Result<()> {
    if !recordings_dir.exists() {
        return Ok(());
    }

    let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(retention_days));
    let cutoff_str = cutoff.format("%Y-%m-%d").to_string();

    let mut host_entries = tokio::fs::read_dir(recordings_dir).await?;
    while let Some(host_entry) = host_entries.next_entry().await? {
        if !host_entry.file_type().await?.is_dir() {
            continue;
        }

        // Collect date dirs for this host so we can prune by age then by size.
        let mut date_dirs: Vec<(String, PathBuf)> = Vec::new();
        let mut date_entries = tokio::fs::read_dir(host_entry.path()).await?;
        while let Some(date_entry) = date_entries.next_entry().await? {
            let name = date_entry.file_name().to_string_lossy().to_string();
            if is_date_dir(&name) && date_entry.file_type().await?.is_dir() {
                // Phase 1: age-based retention.
                if name < cutoff_str {
                    if let Err(e) = tokio::fs::remove_dir_all(date_entry.path()).await {
                        tracing::warn!(
                            "failed to remove expired recording dir {}: {e}",
                            date_entry.path().display()
                        );
                    }
                    continue;
                }
                date_dirs.push((name, date_entry.path()));
            }
        }

        // Phase 2: size-based cap (oldest first).
        let max_bytes = max_size_mb.saturating_mul(1024 * 1024);
        date_dirs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut total = host_dir_size(&host_entry.path()).await;
        for (_name, path) in &date_dirs {
            if total <= max_bytes {
                break;
            }
            let size = dir_size(path).await;
            if let Err(e) = tokio::fs::remove_dir_all(path).await {
                tracing::warn!("failed to remove recording dir {}: {e}", path.display());
            } else {
                total = total.saturating_sub(size);
            }
        }
    }

    Ok(())
}

/// True if `name` looks like a YYYY-MM-DD date directory.
fn is_date_dir(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-'
}

async fn host_dir_size(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut entries = match tokio::fs::read_dir(path).await {
        Ok(e) => e,
        Err(_) => return 0,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.is_ok_and(|ft| ft.is_dir()) {
            total += dir_size(&entry.path()).await;
        }
    }
    total
}

async fn dir_size(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut entries = match tokio::fs::read_dir(path).await {
        Ok(e) => e,
        Err(_) => return 0,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        match entry.metadata().await {
            Ok(m) if m.is_dir() => total += Box::pin(dir_size(&entry.path())).await,
            Ok(m) => total += m.len(),
            Err(_) => {}
        }
    }
    total
}

/// List locally synced recordings, optionally filtered by host, date, and
/// session-name prefix. Scans `{recordings_dir}/{host}/{date}/{file}.cast`.
pub async fn list_local_recordings(
    recordings_dir: &Path,
    host: Option<&str>,
    date: Option<&str>,
    session: Option<&str>,
) -> anyhow::Result<Vec<Value>> {
    let mut results = Vec::new();
    if !recordings_dir.exists() {
        return Ok(results);
    }

    let mut host_entries = tokio::fs::read_dir(recordings_dir).await?;
    while let Some(host_entry) = host_entries.next_entry().await? {
        let host_name = host_entry.file_name().to_string_lossy().to_string();
        if let Some(h) = host {
            if host_name != h {
                continue;
            }
        }
        if !host_entry.file_type().await?.is_dir() {
            continue;
        }

        let mut date_entries = tokio::fs::read_dir(host_entry.path()).await?;
        while let Some(date_entry) = date_entries.next_entry().await? {
            let date_name = date_entry.file_name().to_string_lossy().to_string();
            if let Some(d) = date {
                if date_name != d {
                    continue;
                }
            }
            if !date_entry.file_type().await?.is_dir() {
                continue;
            }

            let mut file_entries = tokio::fs::read_dir(date_entry.path()).await?;
            while let Some(file_entry) = file_entries.next_entry().await? {
                let fname = file_entry.file_name().to_string_lossy().to_string();
                if !fname.ends_with(".cast") {
                    continue;
                }
                if let Some(s) = session {
                    if !fname.starts_with(s) {
                        continue;
                    }
                }
                let file_meta = file_entry.metadata().await?;
                let size_bytes = file_meta.len();

                // Parse session info from filename: {user}_{session}__{pane_id}_{ts}_{random}.cast
                let (user, sess_name, pane_id) = parse_cast_filename(&fname);

                // Read .meta sidecar for duration and timestamp.
                let meta_path = file_entry.path().with_extension("meta");
                let (duration_secs, started_at) = read_meta_sidecar(&meta_path).await;

                results.push(json!({
                    "host": host_name,
                    "date": date_name,
                    "file": fname,
                    "size_bytes": size_bytes,
                    "path": file_entry.path().to_string_lossy(),
                    "user": user,
                    "session": sess_name,
                    "pane": pane_id,
                    "duration_secs": duration_secs,
                    "started_at": started_at,
                }));
            }
        }
    }

    results.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                v["host"].as_str().unwrap_or("").to_string(),
                v["date"].as_str().unwrap_or("").to_string(),
                v["file"].as_str().unwrap_or("").to_string(),
            )
        };
        key(a).cmp(&key(b))
    });

    Ok(results)
}

/// Parse cast filename into (user, session, pane).
/// Format: {user}_{session}__{pane_id}_{timestamp}_{random}.cast
fn parse_cast_filename(fname: &str) -> (String, String, String) {
    let stem = fname.strip_suffix(".cast").unwrap_or(fname);
    // Split at "__" to get {user}_{session} and {pane}_{ts}_{random}
    if let Some((prefix, _rest)) = stem.split_once("__") {
        let pane = prefix.rsplit('_').next().unwrap_or("").to_string();
        let user_session = &prefix[..prefix.len().saturating_sub(pane.len() + 1)];
        if let Some((user, sess)) = user_session.split_once('_') {
            return (user.to_string(), sess.to_string(), pane);
        }
    }
    (String::new(), String::new(), String::new())
}

async fn read_meta_sidecar(meta_path: &std::path::Path) -> (Option<f64>, Option<String>) {
    match tokio::fs::read_to_string(meta_path).await {
        Ok(content) => {
            let meta: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => return (None, None),
            };
            let duration = meta["duration_secs"].as_f64();
            let started_at = meta["closed_at"].as_str().and_then(|closed| {
                chrono::DateTime::parse_from_rfc3339(closed).ok().map(|dt| {
                    let d = duration.unwrap_or(0.0);
                    (dt - chrono::Duration::milliseconds((d * 1000.0) as i64))
                        .format("%Y-%m-%dT%H:%M:%S%:z")
                        .to_string()
                })
            });
            (duration, started_at)
        }
        Err(_) => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_date_dir() {
        assert!(is_date_dir("2026-07-22"));
        assert!(!is_date_dir("2026-7-22"));
        assert!(!is_date_dir("not-a-date"));
        assert!(!is_date_dir("2026_07_22"));
        assert!(!is_date_dir(""));
    }

    #[tokio::test]
    async fn test_list_local_recordings_filters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let h1 = root.join("host1/2026-07-22");
        let h2 = root.join("host2/2026-07-23");
        tokio::fs::create_dir_all(&h1).await.unwrap();
        tokio::fs::create_dir_all(&h2).await.unwrap();
        tokio::fs::write(h1.join("clum-abc.cast"), "data1")
            .await
            .unwrap();
        tokio::fs::write(h1.join("build-xyz.cast"), "data22")
            .await
            .unwrap();
        tokio::fs::write(h2.join("clum-def.cast"), "data333")
            .await
            .unwrap();
        // Non-cast file should be ignored.
        tokio::fs::write(h1.join("clum-abc.meta"), "{}")
            .await
            .unwrap();

        // No filters: all three .cast files.
        let all = list_local_recordings(root, None, None, None).await.unwrap();
        assert_eq!(all.len(), 3);

        // Filter by host.
        let only_h1 = list_local_recordings(root, Some("host1"), None, None)
            .await
            .unwrap();
        assert_eq!(only_h1.len(), 2);
        assert!(only_h1.iter().all(|v| v["host"] == "host1"));

        // Filter by date.
        let only_date = list_local_recordings(root, None, Some("2026-07-23"), None)
            .await
            .unwrap();
        assert_eq!(only_date.len(), 1);
        assert_eq!(only_date[0]["file"], "clum-def.cast");

        // Filter by session prefix.
        let only_session = list_local_recordings(root, None, None, Some("clum"))
            .await
            .unwrap();
        assert_eq!(only_session.len(), 2);

        // Combined host + session.
        let combined = list_local_recordings(root, Some("host1"), None, Some("clum"))
            .await
            .unwrap();
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0]["file"], "clum-abc.cast");
        assert_eq!(combined[0]["size_bytes"], 5);
    }

    #[tokio::test]
    async fn test_list_local_recordings_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let result = list_local_recordings(&missing, None, None, None)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_cleanup_local_recordings_by_age() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let old = root.join("host1/2000-01-01");
        let recent = root.join("host1/2999-01-01");
        tokio::fs::create_dir_all(&old).await.unwrap();
        tokio::fs::create_dir_all(&recent).await.unwrap();
        tokio::fs::write(old.join("a.cast"), "x").await.unwrap();
        tokio::fs::write(recent.join("b.cast"), "y").await.unwrap();

        cleanup_local_recordings(root, 90, 5000).await.unwrap();

        assert!(!old.exists(), "old date dir should be removed");
        assert!(recent.exists(), "recent date dir should be kept");
    }
}
