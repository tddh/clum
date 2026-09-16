//! Asciinema v2 format PTY session recorder.
//!
//! Events flow through a bounded mpsc channel (capacity 4096) to a dedicated
//! writer tokio task. If the channel is full, `try_send` drops events
//! (non-blocking, never blocks the PTY data path).

use std::path::{Path, PathBuf};
use std::time::Instant;

use base64::{engine::general_purpose, Engine as _};
use clum_core::crypto::RecordingEncryptor;
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

/// Channel capacity for cast events.
const CHANNEL_CAPACITY: usize = 4096;

/// fsync threshold: bytes written since last sync.
const FSYNC_BYTE_THRESHOLD: u64 = 64 * 1024; // 64 KB

/// Asciinema v2 event code for PTY output text.
const EVENT_OUTPUT: &str = "o";
/// Asciinema v2 event code for PTY input text.
const EVENT_INPUT: &str = "i";
/// Resize events carry the standard asciinema string payload `"COLSxROWS"`.
const EVENT_RESIZE: &str = "r";
/// Event code for a run of bytes that is confirmed NOT valid UTF-8.
///
/// Deliberately a new event *code*, not a fourth array element: the asciinema
/// v2 spec mandates exactly three elements per event line, and 4-element arrays
/// are rejected by agg / asciinema 3.x (`asc`) / `asciinema cat` / `play`.
/// Unknown event codes, however, are explicitly allowed to be ignored by
/// players, so `"ob"` stays compatible while preserving the exact bytes (base64).
const EVENT_INVALID_BYTES: &str = "ob";
/// Marker written into the output stream where the recorder had to drop events.
const GAP_MARKER: &[u8] = b"[gap]\r\n";

/// Events that can be recorded into a cast file.
pub enum CastEvent {
    Output(Vec<u8>),
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
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
        encryptor: Option<RecordingEncryptor>,
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
            encryptor,
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
        // Emit any deferred `[gap]` marker BEFORE the resuming data so the
        // marker annotates the gap in stream order instead of trailing it.
        self.emit_gap_marker_if_pending();
        if self.tx.try_send(CastEvent::Output(data.to_vec())).is_err() {
            self.gap_pending.store(true, Ordering::Relaxed);
        }
    }

    /// Record PTY input data. Non-blocking; drops event if channel is full.
    pub fn record_input(&self, data: &[u8]) {
        use std::sync::atomic::Ordering;
        self.emit_gap_marker_if_pending();
        if self.tx.try_send(CastEvent::Input(data.to_vec())).is_err() {
            self.gap_pending.store(true, Ordering::Relaxed);
        }
    }

    /// Emit a deferred `[gap]` marker (as an output event) once a prior event
    /// in either direction had to be dropped.
    ///
    /// `gap_pending` is a single flag shared by the output, input and resize
    /// paths — a known conflation: it records "some event was dropped", not
    /// which direction dropped it. The marker is therefore a global annotation
    /// of stream discontinuity and may surface on a different direction than
    /// the one that actually lost the event. Emitting it on every path is what
    /// guarantees it is flushed by whichever direction resumes first.
    ///
    /// The flag is cleared only when the marker is actually enqueued; on failure
    /// it is restored, so a marker that cannot be sent is retried on the next
    /// call rather than being silently lost.
    fn emit_gap_marker_if_pending(&self) {
        use std::sync::atomic::Ordering;
        if !self.gap_pending.swap(false, Ordering::Relaxed) {
            return;
        }
        if self
            .tx
            .try_send(CastEvent::Output(GAP_MARKER.to_vec()))
            .is_err()
        {
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

    /// Clone the event sender so a task that does not own the recorder (e.g.
    /// the interactive *control* task handling client resizes) can enqueue
    /// events. Ordering is preserved because every clone feeds the same
    /// FIFO channel consumed by the single writer task.
    pub fn sender(&self) -> mpsc::Sender<CastEvent> {
        self.tx.clone()
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
    mut encryptor: Option<RecordingEncryptor>,
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
    let header_bytes: Vec<u8> = match encryptor.as_mut() {
        Some(enc) => {
            let mut v = format!("{}\n", enc.header_line()).into_bytes();
            v.extend_from_slice(&enc.write(format!("{header}\n").as_bytes()));
            v
        }
        None => format!("{header}\n").into_bytes(),
    };
    if write_and_track(
        &mut file,
        &mut hasher,
        &mut total_bytes,
        &mut bytes_since_sync,
        &header_bytes,
    )
    .await
    .is_err()
    {
        let _ = done_tx.send(None);
        return;
    }

    // Event loop: process events until Exit or channel close.
    //
    // UTF-8 reassembly state lives here, on the consumer side, so that
    // `record_output` / `record_input` can stay stateless `&self` methods.
    // Output and input are independent byte streams and keep independent
    // pending buffers — they are never concatenated.
    let mut pending_output: Vec<u8> = Vec::new();
    let mut pending_input: Vec<u8> = Vec::new();
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
                let lines = reassemble_events(&mut pending_output, &data, EVENT_OUTPUT, elapsed);
                if write_event_lines(
                    &mut file,
                    &mut hasher,
                    &mut total_bytes,
                    &mut bytes_since_sync,
                    &mut encryptor,
                    &lines,
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            Some(CastEvent::Input(data)) => {
                let elapsed = start.elapsed().as_secs_f64();
                let lines = reassemble_events(&mut pending_input, &data, EVENT_INPUT, elapsed);
                if write_event_lines(
                    &mut file,
                    &mut hasher,
                    &mut total_bytes,
                    &mut bytes_since_sync,
                    &mut encryptor,
                    &lines,
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            Some(CastEvent::Resize { cols, rows }) => {
                let elapsed = start.elapsed().as_secs_f64();
                let line = format!(
                    "[{}, \"{}\", \"{}x{}\"]\n",
                    elapsed, EVENT_RESIZE, cols, rows
                );
                let bytes = encode_chunk(&mut encryptor, line.as_bytes());
                if write_and_track(
                    &mut file,
                    &mut hasher,
                    &mut total_bytes,
                    &mut bytes_since_sync,
                    &bytes,
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            Some(CastEvent::Exit(code)) => {
                let elapsed = start.elapsed().as_secs_f64();
                // An incomplete UTF-8 tail can never be completed now: flush it
                // as an `"ob"` event BEFORE the exit line so no byte is lost.
                let mut lines = flush_pending_bytes(&mut pending_output, elapsed);
                lines.extend(flush_pending_bytes(&mut pending_input, elapsed));
                lines.push(format!("[{}, \"exit\", {}]\n", elapsed, code));
                if write_event_lines(
                    &mut file,
                    &mut hasher,
                    &mut total_bytes,
                    &mut bytes_since_sync,
                    &mut encryptor,
                    &lines,
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
                // Channel closed without an explicit Exit: flush residual tails
                // so they are not silently dropped.
                let elapsed = start.elapsed().as_secs_f64();
                let mut lines = flush_pending_bytes(&mut pending_output, elapsed);
                lines.extend(flush_pending_bytes(&mut pending_input, elapsed));
                let _ = write_event_lines(
                    &mut file,
                    &mut hasher,
                    &mut total_bytes,
                    &mut bytes_since_sync,
                    &mut encryptor,
                    &lines,
                )
                .await;
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

    if let Some(enc) = encryptor.as_mut() {
        let tail = enc.finish();
        if !tail.is_empty() {
            let _ = write_and_track(
                &mut file,
                &mut hasher,
                &mut total_bytes,
                &mut bytes_since_sync,
                &tail,
            )
            .await;
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

fn encode_chunk(encryptor: &mut Option<RecordingEncryptor>, plaintext: &[u8]) -> Vec<u8> {
    match encryptor {
        Some(enc) => enc.write(plaintext),
        None => plaintext.to_vec(),
    }
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

/// Format an asciinema v2 text event line: `[elapsed, "o"|"i", "data"]\n`.
///
/// `text` is valid UTF-8 by construction (the caller reassembles chunks first),
/// so no lossy conversion is performed.
fn format_event_line(elapsed: f64, kind: &str, text: &str) -> String {
    // Use serde_json to properly escape the data string.
    let escaped = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    format!("[{}, \"{}\", {}]\n", elapsed, kind, escaped)
}

/// Format an asciinema v2 invalid-bytes event line:
/// `[elapsed, "ob", "<base64>"]\n`. Still exactly three elements, as v2 requires.
fn format_bytes_event_line(elapsed: f64, bytes: &[u8]) -> String {
    let encoded = general_purpose::STANDARD.encode(bytes);
    format!(
        "[{}, \"{}\", \"{}\"]\n",
        elapsed, EVENT_INVALID_BYTES, encoded
    )
}

/// Reassemble one chunk of a byte stream into lossless asciinema event lines.
///
/// `pending` carries the truncated UTF-8 tail of the previous chunk and is
/// mutated in place. Valid text becomes a `[t, kind, "text"]` line; a
/// confirmed-invalid byte run becomes `[t, "ob", base64]` so the exact bytes
/// survive. Empty events are suppressed. All lines produced here share the same
/// `elapsed`; asciinema v2 permits equal (non-decreasing) timestamps.
fn reassemble_events(pending: &mut Vec<u8>, chunk: &[u8], kind: &str, elapsed: f64) -> Vec<String> {
    let mut lines = Vec::new();
    if chunk.is_empty() && pending.is_empty() {
        return lines;
    }
    pending.extend_from_slice(chunk);
    loop {
        match std::str::from_utf8(pending.as_slice()) {
            Ok(text) => {
                if !text.is_empty() {
                    lines.push(format_event_line(elapsed, kind, text));
                }
                pending.clear();
                break;
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                if valid_up_to > 0 {
                    // SAFETY: bytes before `valid_up_to` are valid UTF-8.
                    let text = std::str::from_utf8(&pending[..valid_up_to]).unwrap_or_default();
                    lines.push(format_event_line(elapsed, kind, text));
                }
                match e.error_len() {
                    Some(bad) => {
                        let end = valid_up_to + bad;
                        lines.push(format_bytes_event_line(elapsed, &pending[valid_up_to..end]));
                        pending.drain(..end);
                    }
                    None => {
                        // Truncated multi-byte sequence at the tail: hold it for
                        // the next chunk, where it may be completed.
                        pending.drain(..valid_up_to);
                        break;
                    }
                }
            }
        }
    }
    lines
}

/// Flush a pending tail that can never be completed as an `"ob"` event.
/// Returns an empty vec when there is nothing pending.
fn flush_pending_bytes(pending: &mut Vec<u8>, elapsed: f64) -> Vec<String> {
    if pending.is_empty() {
        return Vec::new();
    }
    let line = format_bytes_event_line(elapsed, pending);
    pending.clear();
    vec![line]
}

/// Write all formatted lines of one event, updating the hasher and counters.
async fn write_event_lines(
    file: &mut File,
    hasher: &mut Sha256,
    total_bytes: &mut u64,
    bytes_since_sync: &mut u64,
    encryptor: &mut Option<RecordingEncryptor>,
    lines: &[String],
) -> std::io::Result<()> {
    for line in lines {
        let bytes = encode_chunk(encryptor, line.as_bytes());
        write_and_track(file, hasher, total_bytes, bytes_since_sync, &bytes).await?;
    }
    Ok(())
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

        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5, None)
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
    async fn test_recorder_encrypts_when_encryptor_provided() {
        use clum_core::crypto::{
            decrypt_recording, is_encrypted, RecordingEncryptor, RecordingKey,
        };
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("enc.cast");
        let filename = "enc.cast";
        let server = RecordingKey::generate().unwrap();
        let enc =
            RecordingEncryptor::new(&server.public_b64(), &server.key_id(), filename).unwrap();

        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5, Some(enc))
            .await
            .unwrap();
        recorder.record_output(b"secret output");
        recorder.record_input(b"ls\n");
        let meta = recorder.finish(0).await.unwrap();

        let data = tokio::fs::read(&cast_path).await.unwrap();
        assert!(is_encrypted(&data), "on-disk file must be encrypted");

        let mut h = Sha256::new();
        h.update(&data);
        assert_eq!(
            meta.sha256,
            hex::encode(h.finalize()),
            "meta.sha256 must hash the ciphertext bytes on disk"
        );

        let key = server.clone();
        let pt = decrypt_recording(&data, &|id| {
            if id == key.key_id() {
                Some(key.clone())
            } else {
                None
            }
        })
        .unwrap();
        let text = String::from_utf8_lossy(&pt);
        assert!(text.contains("secret output"), "plaintext must round-trip");
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

        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5, None)
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

    fn read_events(path: &Path) -> Vec<(String, serde_json::Value)> {
        let content = std::fs::read_to_string(path).unwrap();
        content
            .lines()
            .skip(1)
            .map(|line| {
                let val: serde_json::Value = serde_json::from_str(line).unwrap();
                let arr = val.as_array().unwrap();
                (arr[1].as_str().unwrap().to_string(), arr[2].clone())
            })
            .collect()
    }

    fn payload_text(events: &[(String, serde_json::Value)], kind: &str) -> String {
        events
            .iter()
            .filter(|(k, _)| k == kind)
            .filter_map(|(_, v)| v.as_str())
            .collect()
    }

    #[tokio::test]
    async fn test_split_multibyte_char_reassembles_losslessly() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("utf8.cast");
        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5, None)
            .await
            .unwrap();

        // "├─┤" = E2 94 9C E2 94 80 E2 94 A4, split mid-character across calls.
        let full = "├─┤".as_bytes();
        recorder.record_output(&full[..2]);
        recorder.record_output(&full[2..5]);
        recorder.record_output(&full[5..]);
        recorder.finish(0).await.unwrap();

        let raw = tokio::fs::read(&cast_path).await.unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(
            !text.contains('\u{FFFD}'),
            "lossless recording must not contain U+FFFD: {text:?}"
        );

        let events = read_events(&cast_path);
        assert_eq!(payload_text(&events, EVENT_OUTPUT), "├─┤");
        assert!(!events.iter().any(|(k, _)| k == EVENT_INVALID_BYTES));
    }

    #[tokio::test]
    async fn test_truncated_tail_defers_until_completed() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("tail.cast");
        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5, None)
            .await
            .unwrap();

        recorder.record_output(&[0xE2]); // truncated tail -> no event yet
        recorder.record_output(&[0x94, 0x9C]); // completes '├'
        recorder.finish(0).await.unwrap();

        let events = read_events(&cast_path);
        let outputs: Vec<String> = events
            .iter()
            .filter(|(k, _)| k == EVENT_OUTPUT)
            .filter_map(|(_, v)| v.as_str())
            .map(str::to_string)
            .collect();
        assert_eq!(outputs, ["├"], "only the completed character is emitted");
        assert!(!events.iter().any(|(k, _)| k == EVENT_INVALID_BYTES));
    }

    #[tokio::test]
    async fn test_invalid_bytes_emit_ob_line_with_exact_base64() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("invalid.cast");
        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5, None)
            .await
            .unwrap();

        let invalid = [0xFF_u8, 0xFE];
        recorder.record_output(b"ok");
        recorder.record_output(&invalid);
        recorder.finish(0).await.unwrap();

        let events = read_events(&cast_path);
        assert_eq!(payload_text(&events, EVENT_OUTPUT), "ok");

        let mut recovered = Vec::new();
        for (kind, value) in &events {
            if kind == EVENT_INVALID_BYTES {
                let decoded = general_purpose::STANDARD
                    .decode(value.as_str().unwrap())
                    .unwrap();
                recovered.extend_from_slice(&decoded);
            }
        }
        assert_eq!(
            recovered, invalid,
            "ob base64 must decode to the exact original bytes"
        );
    }

    #[tokio::test]
    async fn test_gap_marker_precedes_post_gap_data() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("gap_order.cast");
        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5, None)
            .await
            .unwrap();

        // Current-thread runtime: the writer task cannot be polled while we spin
        // without awaiting, so the channel fills deterministically and the last
        // sends are dropped.
        let payload = vec![b'x'; 256];
        for _ in 0..(CHANNEL_CAPACITY * 2) {
            recorder.record_output(&payload);
        }

        for _ in 0..200 {
            if recorder.tx.capacity() > 64 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        recorder.record_output(b"AFTER-GAP");
        recorder.finish(0).await.unwrap();

        let raw = tokio::fs::read_to_string(&cast_path).await.unwrap();
        let gap_idx = raw.find("[gap]").expect("gap marker must be recorded");
        let after_idx = raw
            .find("AFTER-GAP")
            .expect("post-gap data must be recorded");
        assert!(
            gap_idx < after_idx,
            "gap marker must precede the post-gap data"
        );
    }

    #[tokio::test]
    async fn test_gap_marker_not_lost_when_channel_full() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("gap_full.cast");
        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5, None)
            .await
            .unwrap();

        let payload = vec![b'x'; 256];
        for _ in 0..(CHANNEL_CAPACITY + 1) {
            recorder.record_output(&payload);
        }

        // The channel is full, so the marker could not be enqueued. The flag
        // must stay set — a marker that cannot be sent is never silently lost.
        assert!(recorder
            .gap_pending
            .load(std::sync::atomic::Ordering::Relaxed));

        let _ = recorder.finish(0).await;
    }

    #[tokio::test]
    async fn test_resize_event_via_sender_writes_string_form() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("resize.cast");
        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5, None)
            .await
            .unwrap();

        // 生产路径的 resize 发生在 control task，它不持有 recorder（`finish(mut self)`
        // 会取走所有权），只能通过 `sender()` 克隆入队。这里就走这条真实路径，
        // 让 `"r"` 行的格式得到端到端覆盖。
        recorder
            .sender()
            .try_send(CastEvent::Resize {
                cols: 108,
                rows: 31,
            })
            .unwrap();
        recorder.finish(0).await.unwrap();

        let events = read_events(&cast_path);
        let resizes: Vec<String> = events
            .iter()
            .filter(|(k, _)| k == EVENT_RESIZE)
            .filter_map(|(_, v)| v.as_str())
            .map(str::to_string)
            .collect();
        assert_eq!(resizes, ["108x31"]);
    }

    #[tokio::test]
    async fn test_residual_pending_flushed_as_ob_before_exit() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("residual.cast");
        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5, None)
            .await
            .unwrap();

        recorder.record_output(&[0xE2, 0x94]); // incomplete '├'
        recorder.finish(0).await.unwrap();

        let events = read_events(&cast_path);
        let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();
        let n = kinds.len();
        assert_eq!(kinds[n - 1], "exit");
        assert_eq!(kinds[n - 2], EVENT_INVALID_BYTES);
        let decoded = general_purpose::STANDARD
            .decode(events[n - 2].1.as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, vec![0xE2, 0x94]);
    }

    #[tokio::test]
    async fn test_input_and_output_pendings_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("indep.cast");
        let recorder = CastRecorder::start(cast_path.clone(), 80, 24, 5, None)
            .await
            .unwrap();

        // '├' = E2 94 9C on the output stream, '┼' = E2 94 BC on the input
        // stream; both are split mid-character and interleaved.
        recorder.record_output(&[0xE2]);
        recorder.record_input(&[0xE2]);
        recorder.record_output(&[0x94, 0x9C]);
        recorder.record_input(&[0x94, 0xBC]);
        recorder.finish(0).await.unwrap();

        let events = read_events(&cast_path);
        assert_eq!(payload_text(&events, EVENT_OUTPUT), "├");
        assert_eq!(payload_text(&events, EVENT_INPUT), "┼");
        assert!(!events.iter().any(|(k, _)| k == EVENT_INVALID_BYTES));
    }
}
