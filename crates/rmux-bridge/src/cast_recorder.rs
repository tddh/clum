//! Asciinema v2 format PTY session recorder.
//!
//! Events flow through a bounded mpsc channel (capacity 4096) to a dedicated
//! writer tokio task. If the channel is full, `try_send` drops events
//! (non-blocking, never blocks the PTY data path).

use std::path::{Path, PathBuf};
use std::time::Instant;

use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

/// Channel capacity for cast events.
const CHANNEL_CAPACITY: usize = 4096;

/// fsync threshold: bytes written since last sync.
const FSYNC_BYTE_THRESHOLD: u64 = 64 * 1024; // 64 KB

/// Events that can be recorded into a cast file.
pub enum CastEvent {
    Output(Vec<u8>),
    Input(Vec<u8>),
    Exit(i32),
}

/// Metadata computed when a cast recording finishes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CastMeta {
    pub sha256: String,
    pub size_bytes: u64,
    pub duration_secs: f64,
}

/// Asciinema v2 PTY session recorder.
///
/// Recording is non-blocking: `record_output` / `record_input` use `try_send`
/// and silently drop events when the internal channel is full.
pub struct CastRecorder {
    tx: mpsc::Sender<CastEvent>,
    path: PathBuf,
    done_rx: Option<oneshot::Receiver<Option<CastMeta>>>,
    gap_pending: std::sync::atomic::AtomicBool,
}

impl CastRecorder {
    /// Start a new cast recording at `path`.
    ///
    /// Spawns a dedicated writer task that owns the file handle.
    pub async fn start(
        path: PathBuf,
        width: u16,
        height: u16,
        fsync_interval_secs: u64,
    ) -> anyhow::Result<Self> {
        let file = File::create(&path).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&path, perms).await?;
        }
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (done_tx, done_rx) = oneshot::channel();

        tokio::spawn(writer_task(
            file,
            rx,
            done_tx,
            width,
            height,
            fsync_interval_secs,
        ));

        Ok(Self {
            tx,
            path,
            done_rx: Some(done_rx),
            gap_pending: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Record PTY output data. Non-blocking; drops event if channel is full.
    pub fn record_output(&self, data: &[u8]) {
        use std::sync::atomic::Ordering;
        if self.tx.try_send(CastEvent::Output(data.to_vec())).is_err() {
            self.gap_pending.store(true, Ordering::Relaxed);
        } else if self.gap_pending.swap(false, Ordering::Relaxed) {
            let _ = self.tx.try_send(CastEvent::Output(b"[gap]\r\n".to_vec()));
        }
    }

    /// Record PTY input data. Non-blocking; drops event if channel is full.
    pub fn record_input(&self, data: &[u8]) {
        use std::sync::atomic::Ordering;
        if self.tx.try_send(CastEvent::Input(data.to_vec())).is_err() {
            self.gap_pending.store(true, Ordering::Relaxed);
        }
    }

    /// Finish recording: sends the exit event, waits for the writer task to
    /// flush and compute the sha256, returns `CastMeta`.
    pub async fn finish(mut self, exit_code: i32) -> Option<CastMeta> {
        // Send exit event with blocking send to guarantee delivery.
        let _ = self.tx.send(CastEvent::Exit(exit_code)).await;
        // Drop the sender so the writer task sees channel close after Exit.
        drop(self.tx);

        // Wait for the writer task to signal completion.
        let done_rx = self.done_rx.take()?;
        done_rx.await.ok().flatten()
    }

    /// The file path of this cast recording.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The dedicated writer task. Owns the file, processes events, and signals
/// completion via `done_tx`.
async fn writer_task(
    mut file: File,
    mut rx: mpsc::Receiver<CastEvent>,
    done_tx: oneshot::Sender<Option<CastMeta>>,
    width: u16,
    height: u16,
    fsync_interval_secs: u64,
) {
    let start = Instant::now();
    let mut hasher = Sha256::new();
    let mut total_bytes: u64 = 0;
    let mut bytes_since_sync: u64 = 0;
    let mut last_sync = Instant::now();
    let sync_interval = std::time::Duration::from_secs(fsync_interval_secs);

    // Write header line.
    let timestamp = chrono::Utc::now().timestamp();
    let header = serde_json::json!({
        "version": 2,
        "width": width,
        "height": height,
        "timestamp": timestamp,
        "env": {
            "TERM": "xterm-256color",
            "SHELL": "/bin/bash"
        }
    });
    let header_line = format!("{}\n", header);
    if write_and_track(
        &mut file,
        &mut hasher,
        &mut total_bytes,
        &mut bytes_since_sync,
        header_line.as_bytes(),
    )
    .await
    .is_err()
    {
        let _ = done_tx.send(None);
        return;
    }

    // Event loop: process events until Exit or channel close.
    let mut exit_seen = false;
    loop {
        // Check time-based fsync.
        if last_sync.elapsed() >= sync_interval && bytes_since_sync > 0 {
            if let Err(e) = file.sync_all().await {
                tracing::warn!("recording fsync failed: {e}");
            }
            bytes_since_sync = 0;
            last_sync = Instant::now();
        }

        match rx.recv().await {
            Some(CastEvent::Output(data)) => {
                let elapsed = start.elapsed().as_secs_f64();
                let line = format_event_line(elapsed, "o", &data);
                if write_and_track(
                    &mut file,
                    &mut hasher,
                    &mut total_bytes,
                    &mut bytes_since_sync,
                    line.as_bytes(),
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            Some(CastEvent::Input(data)) => {
                let elapsed = start.elapsed().as_secs_f64();
                let line = format_event_line(elapsed, "i", &data);
                if write_and_track(
                    &mut file,
                    &mut hasher,
                    &mut total_bytes,
                    &mut bytes_since_sync,
                    line.as_bytes(),
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            Some(CastEvent::Exit(code)) => {
                let elapsed = start.elapsed().as_secs_f64();
                let line = format!("[{}, \"exit\", {}]\n", elapsed, code);
                if write_and_track(
                    &mut file,
                    &mut hasher,
                    &mut total_bytes,
                    &mut bytes_since_sync,
                    line.as_bytes(),
                )
                .await
                .is_err()
                {
                    break;
                }
                exit_seen = true;
                break;
            }
            None => {
                // Channel closed without explicit Exit.
                break;
            }
        }

        // Byte-threshold fsync.
        if bytes_since_sync >= FSYNC_BYTE_THRESHOLD {
            if let Err(e) = file.sync_all().await {
                tracing::warn!("recording fsync failed: {e}");
            }
            bytes_since_sync = 0;
            last_sync = Instant::now();
        }
    }

    // Final flush and sync.
    if let Err(e) = file.flush().await {
        tracing::warn!("recording final flush failed: {e}");
    }
    if let Err(e) = file.sync_all().await {
        tracing::warn!("recording final sync failed: {e}");
    }

    let duration_secs = start.elapsed().as_secs_f64();
    let hash = hasher.finalize();
    let sha256 = hex::encode(hash);

    let meta = if exit_seen || total_bytes > 0 {
        Some(CastMeta {
            sha256,
            size_bytes: total_bytes,
            duration_secs,
        })
    } else {
        None
    };

    let _ = done_tx.send(meta);
}

/// Write data to file, update hasher and byte counters.
async fn write_and_track(
    file: &mut File,
    hasher: &mut Sha256,
    total_bytes: &mut u64,
    bytes_since_sync: &mut u64,
    data: &[u8],
) -> std::io::Result<()> {
    file.write_all(data).await?;
    hasher.update(data);
    *total_bytes += data.len() as u64;
    *bytes_since_sync += data.len() as u64;
    Ok(())
}

/// Format an asciinema v2 event line: `[elapsed, "o"|"i", "data"]\n`
fn format_event_line(elapsed: f64, kind: &str, data: &[u8]) -> String {
    // Use serde_json to properly escape the data string.
    let data_str = String::from_utf8_lossy(data);
    let escaped = serde_json::to_string(&data_str).unwrap_or_else(|_| "\"\"".to_string());
    format!("[{}, \"{}\", {}]\n", elapsed, kind, escaped)
}

/// Write a `.meta` sidecar JSON file next to the cast file and (on Linux)
/// set the append-only attribute via `chattr +a`.
pub async fn finalize_cast(cast_path: &Path, meta: &CastMeta) -> anyhow::Result<()> {
    let meta_path = cast_path.with_extension("meta");

    let closed_at = chrono::Utc::now().to_rfc3339();
    let meta_json = serde_json::json!({
        "sha256": meta.sha256,
        "synced": false,
        "closed_at": closed_at,
        "duration_secs": meta.duration_secs,
        "size_bytes": meta.size_bytes,
    });

    let content = serde_json::to_string_pretty(&meta_json)?;
    tokio::fs::write(&meta_path, content.as_bytes()).await?;

    // Best-effort: set append-only attribute on the cast file (Linux).
    set_append_only(cast_path);

    Ok(())
}

/// Set the append-only flag (`FS_APPEND_FL`) on a file. Linux only, best-effort.
#[cfg(target_os = "linux")]
fn set_append_only(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // FS_IOC_GETFLAGS / FS_IOC_SETFLAGS ioctl numbers and FS_APPEND_FL flag.
    const FS_IOC_GETFLAGS: libc::Ioctl = 0x8008_6601_u64 as libc::Ioctl;
    const FS_IOC_SETFLAGS: libc::Ioctl = 0x4008_6602_u64 as libc::Ioctl;
    const FS_APPEND_FL: libc::c_int = 0x0000_0020;

    let c_path = match CString::new(path.as_os_str().as_bytes()) {
        Ok(p) => p,
        Err(_) => return,
    };

    unsafe {
        let fd = libc::open(c_path.as_ptr(), libc::O_RDONLY);
        if fd < 0 {
            return;
        }

        let mut flags: libc::c_int = 0;
        if libc::ioctl(fd, FS_IOC_GETFLAGS, &mut flags) == 0 {
            flags |= FS_APPEND_FL;
            let _ = libc::ioctl(fd, FS_IOC_SETFLAGS, &flags);
        }

        libc::close(fd);
    }
}

/// No-op on non-Linux platforms.
#[cfg(not(target_os = "linux"))]
fn set_append_only(_path: &Path) {
    // chattr is Linux-specific; skip silently.
}

/// Clear the append-only flag (`FS_APPEND_FL`) on a file. Linux only, best-effort.
#[cfg(target_os = "linux")]
fn clear_append_only(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const FS_IOC_GETFLAGS: libc::Ioctl = 0x8008_6601_u64 as libc::Ioctl;
    const FS_IOC_SETFLAGS: libc::Ioctl = 0x4008_6602_u64 as libc::Ioctl;
    const FS_APPEND_FL: libc::c_int = 0x0000_0020;

    let c_path = match CString::new(path.as_os_str().as_bytes()) {
        Ok(p) => p,
        Err(_) => return,
    };

    unsafe {
        let fd = libc::open(c_path.as_ptr(), libc::O_RDONLY);
        if fd < 0 {
            return;
        }

        let mut flags: libc::c_int = 0;
        if libc::ioctl(fd, FS_IOC_GETFLAGS, &mut flags) == 0 {
            flags &= !FS_APPEND_FL;
            let _ = libc::ioctl(fd, FS_IOC_SETFLAGS, &flags);
        }

        libc::close(fd);
    }
}

/// No-op on non-Linux platforms.
#[cfg(not(target_os = "linux"))]
fn clear_append_only(_path: &Path) {
    // chattr is Linux-specific; skip silently.
}

/// Sum file sizes in a directory (non-recursive).
async fn dir_size(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut entries = match tokio::fs::read_dir(path).await {
        Ok(e) => e,
        Err(_) => return 0,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(meta) = entry.metadata().await {
            if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// Sum all date directory sizes under the recording directory.
async fn total_recording_size(recording_dir: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut entries = match tokio::fs::read_dir(recording_dir).await {
        Ok(e) => e,
        Err(_) => return 0,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(meta) = entry.metadata().await {
            if meta.is_dir() {
                total += dir_size(&entry.path()).await;
            }
        }
    }
    total
}

/// Remove `chattr +a` from .cast files in a directory, then remove the directory tree.
async fn remove_dir_all_with_chattr(path: &Path) -> std::io::Result<()> {
    let mut entries = tokio::fs::read_dir(path).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let entry_path = entry.path();
        if entry_path.extension().is_some_and(|ext| ext == "cast") {
            clear_append_only(&entry_path);
        }
    }
    tokio::fs::remove_dir_all(path).await
}

/// Clean up old recording directories.
///
/// Phase 1: Delete date directories (format YYYY-MM-DD) older than `retention_days`.
/// Phase 2: If total size still exceeds `max_size_mb`, delete oldest date directories
/// until under the limit.
///
/// Returns `(files_deleted, bytes_freed)`.
pub async fn cleanup_recordings(
    recording_dir: &Path,
    retention_days: u32,
    max_size_mb: u64,
) -> anyhow::Result<(usize, u64)> {
    let mut files_deleted: usize = 0;
    let mut bytes_freed: u64 = 0;

    // Compute cutoff date string (YYYY-MM-DD).
    let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(retention_days));
    let cutoff_str = cutoff.format("%Y-%m-%d").to_string();

    // Collect date directories sorted by name (lexicographic = chronological).
    let mut date_dirs: Vec<(String, PathBuf)> = Vec::new();
    let mut entries = tokio::fs::read_dir(recording_dir).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        // Validate YYYY-MM-DD format (10 chars, dashes at positions 4 and 7).
        if name.len() == 10
            && name.as_bytes()[4] == b'-'
            && name.as_bytes()[7] == b'-'
            && entry.file_type().await.is_ok_and(|ft| ft.is_dir())
        {
            date_dirs.push((name, entry.path()));
        }
    }
    date_dirs.sort_by(|a, b| a.0.cmp(&b.0));

    // Phase 1: Delete directories older than retention cutoff.
    let mut remaining: Vec<(String, PathBuf)> = Vec::new();
    for (name, path) in &date_dirs {
        if name.as_str() < cutoff_str.as_str() {
            let size = dir_size(path).await;
            if let Err(e) = remove_dir_all_with_chattr(path).await {
                tracing::warn!(
                    "failed to remove old recording dir {}: {}",
                    path.display(),
                    e
                );
                remaining.push((name.clone(), path.clone()));
            } else {
                files_deleted += 1;
                bytes_freed += size;
                tracing::debug!(dir = %name, bytes = size, "removed expired recording dir");
            }
        } else {
            remaining.push((name.clone(), path.clone()));
        }
    }

    // Phase 2: If total size exceeds max, delete oldest remaining directories.
    let max_bytes = max_size_mb * 1024 * 1024;
    let mut current_size = total_recording_size(recording_dir).await;

    for (name, path) in &remaining {
        if current_size <= max_bytes {
            break;
        }
        let size = dir_size(path).await;
        if let Err(e) = remove_dir_all_with_chattr(path).await {
            tracing::warn!("failed to remove recording dir {}: {}", path.display(), e);
        } else {
            files_deleted += 1;
            bytes_freed += size;
            current_size = current_size.saturating_sub(size);
            tracing::debug!(dir = %name, bytes = size, "removed recording dir for size limit");
        }
    }

    if files_deleted > 0 {
        tracing::info!(files_deleted, bytes_freed, "recording cleanup completed");
    }

    Ok((files_deleted, bytes_freed))
}

/// List all recordings with `synced == false` in their `.meta` file.
///
/// Scans date directories under `recording_dir`, reads each `.meta` file,
/// and returns entries where `synced` is false.
pub async fn list_unsynced(recording_dir: &Path) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut results = Vec::new();
    if !recording_dir.exists() {
        return Ok(results);
    }

    let mut date_entries = tokio::fs::read_dir(recording_dir).await?;
    while let Ok(Some(date_entry)) = date_entries.next_entry().await {
        let date_name = date_entry.file_name().to_string_lossy().to_string();
        // Only process YYYY-MM-DD directories.
        if date_name.len() != 10
            || date_name.as_bytes()[4] != b'-'
            || date_name.as_bytes()[7] != b'-'
        {
            continue;
        }
        if !date_entry.file_type().await.is_ok_and(|ft| ft.is_dir()) {
            continue;
        }

        let date_path = date_entry.path();
        let mut meta_entries = tokio::fs::read_dir(&date_path).await?;
        while let Ok(Some(meta_entry)) = meta_entries.next_entry().await {
            let meta_name = meta_entry.file_name().to_string_lossy().to_string();
            if !meta_name.ends_with(".meta") {
                continue;
            }

            let meta_path = meta_entry.path();
            let content = match tokio::fs::read_to_string(&meta_path).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            let meta: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if meta["synced"].as_bool() == Some(false) {
                let cast_name = meta_name.trim_end_matches(".meta").to_string() + ".cast";
                let cast_path = date_path.join(&cast_name);
                let size_bytes = tokio::fs::metadata(&cast_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or_else(|_| meta["size_bytes"].as_u64().unwrap_or(0));

                results.push(serde_json::json!({
                    "file": cast_name,
                    "date": date_name,
                    "path": cast_path.to_string_lossy(),
                    "size_bytes": size_bytes,
                    "sha256": meta["sha256"].as_str().unwrap_or(""),
                }));
            }
        }
    }

    Ok(results)
}

/// Mark a recording as synced in its `.meta` file.
///
/// Finds `{recording_dir}/{date}/{file_name}`, derives the `.meta` path,
/// reads it, sets `"synced": true`, and writes it back.
pub async fn mark_synced(recording_dir: &Path, file_name: &str, date: &str) -> anyhow::Result<()> {
    if file_name.contains('/') || file_name.contains('\\') || file_name.contains("..") {
        anyhow::bail!("unsafe file_name in mark_synced: '{file_name}'");
    }
    if date.contains('/') || date.contains('\\') || date.contains("..") {
        anyhow::bail!("unsafe date in mark_synced: '{date}'");
    }
    let meta_path = recording_dir
        .join(date)
        .join(file_name)
        .with_extension("meta");
    let content = tokio::fs::read_to_string(&meta_path).await?;
    let mut meta: serde_json::Value = serde_json::from_str(&content)?;
    meta["synced"] = serde_json::json!(true);
    tokio::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_recorder_creates_valid_cast_file() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("test.cast");

        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5)
            .await
            .unwrap();

        recorder.record_output(b"hello world");
        recorder.record_input(b"ls -la\n");
        recorder.record_output(b"file1.txt\r\nfile2.txt\r\n");

        let meta = recorder.finish(0).await;
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert!(!meta.sha256.is_empty());
        assert!(meta.size_bytes > 0);
        assert!(meta.duration_secs >= 0.0);

        // Read and validate the cast file.
        let content = tokio::fs::read_to_string(&cast_path).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();

        // At least header + 3 events + exit = 5 lines.
        assert!(lines.len() >= 5, "expected >= 5 lines, got {}", lines.len());

        // Validate header.
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 80);
        assert_eq!(header["height"], 24);
        assert!(header["timestamp"].is_number());
        assert_eq!(header["env"]["TERM"], "xterm-256color");
        assert_eq!(header["env"]["SHELL"], "/bin/bash");

        // Validate event lines are valid JSON arrays.
        for line in &lines[1..] {
            let val: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(
                val.is_array(),
                "event line should be a JSON array: {}",
                line
            );
            let arr = val.as_array().unwrap();
            assert!(arr[0].is_number(), "first element should be timestamp");
            assert!(arr[1].is_string(), "second element should be event type");
        }

        // Last line should be exit event.
        let last: serde_json::Value = serde_json::from_str(lines[lines.len() - 1]).unwrap();
        let last_arr = last.as_array().unwrap();
        assert_eq!(last_arr[1], "exit");
        assert_eq!(last_arr[2], 0);
    }

    #[tokio::test]
    async fn test_finalize_cast_writes_meta() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("session.cast");

        // Create a dummy cast file.
        tokio::fs::write(&cast_path, b"dummy cast content\n")
            .await
            .unwrap();

        let meta = CastMeta {
            sha256: "abcdef1234567890".to_string(),
            size_bytes: 1024,
            duration_secs: 42.5,
        };

        finalize_cast(&cast_path, &meta).await.unwrap();

        // Verify .meta sidecar exists and has correct content.
        let meta_path = cast_path.with_extension("meta");
        assert!(meta_path.exists(), ".meta file should exist");

        let meta_content = tokio::fs::read_to_string(&meta_path).await.unwrap();
        let meta_json: serde_json::Value = serde_json::from_str(&meta_content).unwrap();

        assert_eq!(meta_json["sha256"], "abcdef1234567890");
        assert_eq!(meta_json["synced"], false);
        assert_eq!(meta_json["duration_secs"], 42.5);
        assert_eq!(meta_json["size_bytes"], 1024);
        assert!(meta_json["closed_at"].is_string());

        // Verify closed_at is valid RFC3339.
        let closed_at = meta_json["closed_at"].as_str().unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(closed_at).is_ok(),
            "closed_at should be valid RFC3339: {}",
            closed_at
        );
    }

    #[tokio::test]
    async fn test_recorder_nonblocking_on_full_channel() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("flood.cast");

        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5)
            .await
            .unwrap();

        // Flood with 10000 events — must not panic or block.
        let payload = vec![b'x'; 256];
        for i in 0..10_000 {
            if i % 2 == 0 {
                recorder.record_output(&payload);
            } else {
                recorder.record_input(&payload);
            }
        }

        // Finish should still work even if some events were dropped.
        let meta = recorder.finish(0).await;
        assert!(meta.is_some());

        // The file should exist and be valid (at least header + exit).
        let content = tokio::fs::read_to_string(&cast_path).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert!(lines.len() >= 2, "at least header + exit line");

        // Header is valid JSON.
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["version"], 2);
    }

    #[tokio::test]
    async fn test_cleanup_removes_old_directories() {
        let dir = tempfile::tempdir().unwrap();
        let recordings = dir.path().join("recordings");
        tokio::fs::create_dir_all(&recordings).await.unwrap();

        // Create an old date dir (2020-01-01) and a recent date dir (2026-07-22).
        let old_dir = recordings.join("2020-01-01");
        let new_dir = recordings.join("2026-07-22");
        tokio::fs::create_dir_all(&old_dir).await.unwrap();
        tokio::fs::create_dir_all(&new_dir).await.unwrap();

        // Put dummy .cast files in each.
        tokio::fs::write(old_dir.join("session1.cast"), b"old cast data\n")
            .await
            .unwrap();
        tokio::fs::write(old_dir.join("session1.meta"), b"{}\n")
            .await
            .unwrap();
        tokio::fs::write(new_dir.join("session2.cast"), b"new cast data\n")
            .await
            .unwrap();
        tokio::fs::write(new_dir.join("session2.meta"), b"{}\n")
            .await
            .unwrap();

        // Run cleanup with retention_days=90 — old dir should be deleted.
        let (deleted, freed) = cleanup_recordings(&recordings, 90, 1024).await.unwrap();

        assert_eq!(deleted, 1, "should delete exactly 1 directory");
        assert!(freed > 0, "should report bytes freed");
        assert!(!old_dir.exists(), "old dir should be removed");
        assert!(new_dir.exists(), "new dir should remain");
    }

    #[tokio::test]
    async fn test_list_unsynced_and_mark_synced() {
        let dir = tempfile::tempdir().unwrap();
        let rec_dir = dir.path().join("recordings");
        let date_dir = rec_dir.join("2026-07-22");
        tokio::fs::create_dir_all(&date_dir).await.unwrap();

        tokio::fs::write(date_dir.join("test.cast"), "data")
            .await
            .unwrap();
        tokio::fs::write(
            date_dir.join("test.meta"),
            r#"{"sha256": "abc", "synced": false, "size_bytes": 4}"#,
        )
        .await
        .unwrap();

        let unsynced = list_unsynced(&rec_dir).await.unwrap();
        assert_eq!(unsynced.len(), 1);
        assert_eq!(unsynced[0]["file"], "test.cast");
        assert_eq!(unsynced[0]["date"], "2026-07-22");

        mark_synced(&rec_dir, "test.cast", "2026-07-22")
            .await
            .unwrap();

        let unsynced_after = list_unsynced(&rec_dir).await.unwrap();
        assert_eq!(unsynced_after.len(), 0);
    }
}
