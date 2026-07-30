//! File upload/download via QUIC transport.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;
use yunying_core::types::HostConfig;

use crate::registry::BridgeRegistry;

const MAX_UPLOAD_CONCURRENCY: usize = 16;
const MAX_FILE_SIZE: usize = 2 * 1024 * 1024 * 1024; // 2 GB
const COPY_BUF_SIZE: usize = 1024 * 1024; // 1 MB — 与 bridge CHUNK_SIZE 对齐

const STREAM_UPLOAD: u8 = 0x02;
const STREAM_DOWNLOAD: u8 = 0x03;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OverwriteMode {
    Overwrite = 0x01,
    Skip = 0x02,
    Rename = 0x03,
    NoClobber = 0x04,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileResult {
    pub path: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

async fn get_conn(
    host: &HostConfig,
    registry: &Arc<BridgeRegistry>,
    ca_cert_path: &str,
) -> Result<quinn::Connection> {
    if let Some(bridge) = registry.get(&host.name).await {
        if bridge.conn.close_reason().is_none() {
            return Ok(bridge.conn.clone());
        }
    }
    let (conn, _auth_send, _auth_recv) = crate::transport::connect_to_bridge_quic(
        &host.bridge_addr,
        &host.bridge_token,
        ca_cert_path,
    )
    .await?;
    Ok(conn)
}

#[allow(clippy::too_many_arguments)]
pub async fn upload_file(
    host: &HostConfig,
    local_path: &str,
    remote_path: &str,
    ca_cert_path: &str,
    overwrite: OverwriteMode,
    exclude: &[String],
    progress: &mut crate::progress::ProgressReporter,
    registry: &Arc<BridgeRegistry>,
) -> Result<Vec<FileResult>> {
    let meta = tokio::fs::metadata(local_path)
        .await
        .with_context(|| format!("failed to access: {}", local_path))?;

    if meta.is_dir() {
        upload_dir(
            host,
            local_path,
            remote_path,
            ca_cert_path,
            overwrite,
            exclude,
            progress,
            registry,
        )
        .await
    } else {
        let size = meta.len() as usize;
        if size > MAX_FILE_SIZE {
            bail!("file too large: {} bytes (max {})", size, MAX_FILE_SIZE);
        }
        let result = upload_single(
            host,
            local_path,
            remote_path,
            ca_cert_path,
            overwrite,
            progress,
            registry,
        )
        .await?;
        Ok(vec![result])
    }
}

async fn upload_single(
    host: &HostConfig,
    local_path: &str,
    remote_path: &str,
    ca_cert_path: &str,
    overwrite: OverwriteMode,
    progress: &mut crate::progress::ProgressReporter,
    registry: &Arc<BridgeRegistry>,
) -> Result<FileResult> {
    let meta = tokio::fs::metadata(local_path).await?;
    let file_size = meta.len();

    let conn = get_conn(host, registry, ca_cert_path).await?;
    let (mut send, mut recv) = conn.open_bi().await?;

    send.write_all(&[STREAM_UPLOAD]).await?;
    send.write_all(&[overwrite as u8]).await?;
    send.write_all(&(remote_path.len() as u16).to_le_bytes())
        .await?;
    send.write_all(remote_path.as_bytes()).await?;
    send.write_all(&file_size.to_le_bytes()).await?;

    let mut file = tokio::fs::File::open(local_path).await?;
    copy_with_buf(&mut file, &mut send, file_size, progress).await?;
    send.finish()?;

    let mut code = [0u8; 1];
    recv.read_exact(&mut code).await?;
    let mut written = [0u8; 8];
    recv.read_exact(&mut written).await?;
    let mut sha256 = [0u8; 32];
    recv.read_exact(&mut sha256).await?;

    match code[0] {
        0x00 => Ok(FileResult {
            path: remote_path.to_string(),
            status: "uploaded".into(),
            size: Some(u64::from_le_bytes(written)),
            sha256: Some(hex::encode(sha256)),
            error: None,
        }),
        0x01 => Ok(FileResult {
            path: remote_path.to_string(),
            status: "skipped".into(),
            size: None,
            sha256: None,
            error: None,
        }),
        _ => bail!("upload failed: server code 0x{:02x}", code[0]),
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_dir(
    host: &HostConfig,
    local_path: &str,
    remote_base: &str,
    ca_cert_path: &str,
    overwrite: OverwriteMode,
    exclude: &[String],
    progress: &crate::progress::ProgressReporter,
    registry: &Arc<BridgeRegistry>,
) -> Result<Vec<FileResult>> {
    let base = Path::new(local_path).to_path_buf();
    let mut files = Vec::new();
    collect_files(&base, &base, remote_base, exclude, &mut files, 0).await?;
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let conn = Arc::new(get_conn(host, registry, ca_cert_path).await?);

    let semaphore = Arc::new(Semaphore::new(MAX_UPLOAD_CONCURRENCY));
    let mut handles = Vec::new();

    for (local, remote) in files {
        let conn = conn.clone();
        let permit = semaphore.clone().acquire_owned().await?;
        let mut file_progress = progress.clone();

        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let meta = tokio::fs::metadata(&local).await?;
            let file_size = meta.len();

            let (mut send, mut recv) = conn.open_bi().await?;

            send.write_all(&[STREAM_UPLOAD]).await?;
            send.write_all(&[overwrite as u8]).await?;
            send.write_all(&(remote.len() as u16).to_le_bytes()).await?;
            send.write_all(remote.as_bytes()).await?;
            send.write_all(&file_size.to_le_bytes()).await?;

            let mut file = tokio::fs::File::open(&local).await?;
            copy_with_buf(&mut file, &mut send, file_size, &mut file_progress).await?;
            send.finish()?;

            let mut code = [0u8; 1];
            recv.read_exact(&mut code).await?;
            let mut written = [0u8; 8];
            recv.read_exact(&mut written).await?;
            let mut sha256 = [0u8; 32];
            recv.read_exact(&mut sha256).await?;

            Ok::<_, anyhow::Error>(match code[0] {
                0x00 => FileResult {
                    path: remote.clone(),
                    status: "uploaded".into(),
                    size: Some(u64::from_le_bytes(written)),
                    sha256: Some(hex::encode(sha256)),
                    error: None,
                },
                0x01 => FileResult {
                    path: remote.clone(),
                    status: "skipped".into(),
                    size: None,
                    sha256: None,
                    error: None,
                },
                _ => bail!("upload failed: server code 0x{:02x}", code[0]),
            })
        }));
    }

    let mut results = Vec::new();
    for h in handles {
        match h.await? {
            Ok(r) => results.push(r),
            Err(e) => {
                tracing::warn!("upload failed: {}", e);
                results.push(FileResult {
                    status: "failed".to_string(),
                    path: String::new(),
                    size: None,
                    sha256: None,
                    error: Some(e.to_string()),
                });
            }
        }
    }
    Ok(results)
}

pub async fn download_file(
    host: &HostConfig,
    remote_path: &str,
    local_path: &str,
    ca_cert_path: &str,
    progress: &mut crate::progress::ProgressReporter,
    registry: &Arc<BridgeRegistry>,
) -> Result<Vec<FileResult>> {
    let conn = get_conn(host, registry, ca_cert_path).await?;
    let (mut send, mut recv) = conn.open_bi().await?;

    send.write_all(&[STREAM_DOWNLOAD]).await?;
    send.write_all(&(remote_path.len() as u16).to_le_bytes())
        .await?;
    send.write_all(remote_path.as_bytes()).await?;
    send.finish()?;

    let mut code = [0u8; 1];
    recv.read_exact(&mut code).await?;

    match code[0] {
        0x00 => {
            let result = read_single_file(&mut recv, local_path, progress).await?;
            Ok(vec![result])
        }
        0x04 => {
            let results = read_directory(&mut recv, local_path, progress).await?;
            Ok(results)
        }
        0x02 => {
            let mut msg_len = [0u8; 2];
            recv.read_exact(&mut msg_len).await?;
            let len = u16::from_le_bytes(msg_len) as usize;
            let mut msg = vec![0u8; len];
            recv.read_exact(&mut msg).await?;
            bail!("download failed: {}", String::from_utf8_lossy(&msg));
        }
        _ => bail!("unknown response code: 0x{:02x}", code[0]),
    }
}

async fn read_single_file(
    recv: &mut quinn::RecvStream,
    local_path: &str,
    progress: &mut crate::progress::ProgressReporter,
) -> Result<FileResult> {
    let mut size_buf = [0u8; 8];
    recv.read_exact(&mut size_buf).await?;
    let file_size = u64::from_le_bytes(size_buf) as usize;
    if file_size > MAX_FILE_SIZE {
        bail!(
            "file too large: {} bytes (max {})",
            file_size,
            MAX_FILE_SIZE
        );
    }

    if let Some(parent) = Path::new(local_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut file = tokio::fs::File::create(local_path)
        .await
        .with_context(|| format!("failed to create: {}", local_path))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let mut received = 0u64;
    let mut remaining = file_size as u64;
    while remaining > 0 {
        let to_read = std::cmp::min(buf.len() as u64, remaining) as usize;
        let n = recv.read(&mut buf[..to_read]).await?.unwrap_or(0);
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).await?;
        received += n as u64;
        remaining -= n as u64;
        progress
            .report(received, file_size as u64, "downloading")
            .await;
    }
    progress
        .report(file_size as u64, file_size as u64, "done")
        .await;
    file.flush().await?;

    if received != file_size as u64 {
        bail!(
            "truncated transfer: expected {} bytes, got {}",
            file_size,
            received
        );
    }

    let hash: [u8; 32] = hasher.finalize().into();

    Ok(FileResult {
        path: local_path.to_string(),
        status: "downloaded".into(),
        size: Some(file_size as u64),
        sha256: Some(hex::encode(hash)),
        error: None,
    })
}

async fn read_directory(
    recv: &mut quinn::RecvStream,
    local_base: &str,
    progress: &mut crate::progress::ProgressReporter,
) -> Result<Vec<FileResult>> {
    let mut count_buf = [0u8; 4];
    recv.read_exact(&mut count_buf).await?;
    let file_count = u32::from_le_bytes(count_buf) as usize;

    tokio::fs::create_dir_all(local_base)
        .await
        .with_context(|| format!("failed to create directory: {}", local_base))?;

    let mut results = Vec::with_capacity(file_count);
    for i in 0..file_count {
        let mut path_len_buf = [0u8; 2];
        recv.read_exact(&mut path_len_buf).await?;
        let path_len = u16::from_le_bytes(path_len_buf) as usize;
        let mut path_buf = vec![0u8; path_len];
        recv.read_exact(&mut path_buf).await?;
        let rel_path = String::from_utf8_lossy(&path_buf).to_string();

        // 验证 rel_path 安全性：拒绝路径穿越和绝对路径
        if rel_path.contains("..") || rel_path.starts_with('/') {
            bail!(
                "unsafe relative path from bridge: '{}' (contains '..' or is absolute)",
                rel_path
            );
        }

        let mut size_buf = [0u8; 8];
        recv.read_exact(&mut size_buf).await?;
        let file_size = u64::from_le_bytes(size_buf) as usize;
        if file_size > MAX_FILE_SIZE {
            bail!(
                "file too large: {} bytes (max {})",
                file_size,
                MAX_FILE_SIZE
            );
        }

        let local_path = format!("{}/{}", local_base, rel_path);
        if let Some(parent) = Path::new(&local_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut file = tokio::fs::File::create(&local_path)
            .await
            .with_context(|| format!("failed to create: {}", local_path))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; COPY_BUF_SIZE];
        let mut remaining = file_size as u64;
        while remaining > 0 {
            let to_read = std::cmp::min(buf.len() as u64, remaining) as usize;
            let n = recv.read(&mut buf[..to_read]).await?.unwrap_or(0);
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).await?;
            remaining -= n as u64;
        }
        file.flush().await?;
        progress
            .report((i + 1) as u64, file_count as u64, "downloading dir")
            .await;

        let hash: [u8; 32] = hasher.finalize().into();

        results.push(FileResult {
            path: rel_path,
            status: "downloaded".into(),
            size: Some(file_size as u64),
            sha256: Some(hex::encode(hash)),
            error: None,
        });
    }

    Ok(results)
}

// ─── helper functions ───

async fn copy_with_buf<R, W>(
    reader: &mut R,
    writer: &mut W,
    total_size: u64,
    progress: &mut crate::progress::ProgressReporter,
) -> std::io::Result<u64>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).await?;
        total += n as u64;
        progress.report(total, total_size, "transferring").await;
    }
    progress.report(total_size, total_size, "done").await;
    Ok(total)
}

async fn collect_files(
    base: &Path,
    dir: &Path,
    remote_base: &str,
    exclude: &[String],
    files: &mut Vec<(PathBuf, String)>,
    depth: u32,
) -> Result<()> {
    if depth > 64 {
        return Err(anyhow::anyhow!(
            "directory too deep (>64): {}",
            dir.display()
        ));
    }
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let rel = path.strip_prefix(base)?.to_string_lossy().to_string();
        let remote = format!("{}/{}", remote_base.trim_end_matches('/'), rel);
        if should_exclude(&rel, exclude) {
            continue;
        }
        if path.is_dir() {
            Box::pin(collect_files(
                base,
                &path,
                remote_base,
                exclude,
                files,
                depth + 1,
            ))
            .await?;
        } else {
            files.push((path, remote));
        }
    }
    Ok(())
}

fn should_exclude(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        glob::Pattern::new(p)
            .map(|pat| pat.matches(path))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_should_exclude_glob() {
        let patterns: Vec<String> = vec!["*.log".into(), ".git/*".into(), "target/*".into()];
        assert!(should_exclude("app.log", &patterns));
        assert!(should_exclude("sub/app.log", &patterns));
        assert!(should_exclude(".git/config", &patterns));
        assert!(should_exclude("target/debug/foo", &patterns));
        assert!(!should_exclude("main.rs", &patterns));
    }

    #[test]
    fn test_should_exclude_empty() {
        assert!(!should_exclude("anything", &[]));
    }

    #[tokio::test]
    async fn test_collect_files_with_exclude() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("skip.log"), "error").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "data").unwrap();

        let mut files = Vec::new();
        let exclude: Vec<String> = vec!["*.log".into(), ".git/*".into()];
        collect_files(dir.path(), dir.path(), "/remote", &exclude, &mut files, 0)
            .await
            .unwrap();

        let names: Vec<&str> = files.iter().map(|(_, r)| r.as_str()).collect();
        assert!(names.contains(&"/remote/keep.rs"));
        assert!(!names.contains(&"/remote/skip.log"));
        assert!(!names.contains(&"/remote/.git/config"));
    }

    #[test]
    fn test_rel_path_rejects_dotdot() {
        let bad_paths = ["../etc/passwd", "foo/../../etc/shadow", ".."];
        for p in bad_paths {
            assert!(p.contains(".."), "should detect path traversal in: {}", p);
        }
    }

    #[test]
    fn test_rel_path_rejects_absolute() {
        let bad_paths = ["/etc/passwd", "/root/.ssh/id_rsa"];
        for p in bad_paths {
            assert!(p.starts_with('/'), "should detect absolute path: {}", p);
        }
    }

    #[test]
    fn test_rel_path_accepts_normal() {
        let good_paths = ["file.txt", "subdir/file.txt", "a/b/c/d.rs"];
        for p in good_paths {
            assert!(
                !p.contains("..") && !p.starts_with('/'),
                "should accept normal path: {}",
                p
            );
        }
    }
}
