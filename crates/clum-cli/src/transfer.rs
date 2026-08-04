use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const STREAM_UPLOAD: u8 = 0x02;
const STREAM_DOWNLOAD: u8 = 0x03;
const MODE_OVERWRITE: u8 = 0x01;
const CHUNK_SIZE: usize = 1024 * 1024;
const MAX_UPLOAD_CONCURRENCY: usize = 16;
const MAX_DIR_DEPTH: u32 = 64;

async fn collect_files(
    base: &Path,
    dir: &Path,
    remote_base: &str,
    exclude: &[String],
    files: &mut Vec<(PathBuf, String)>,
    depth: u32,
) -> Result<()> {
    if depth > MAX_DIR_DEPTH {
        bail!("directory too deep (>64): {}", dir.display());
    }
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let rel = path.strip_prefix(base)?.to_string_lossy().to_string();
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
            let remote = format!("{}/{}", remote_base.trim_end_matches('/'), rel);
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

fn validate_rel_path(rel: &str) -> Result<()> {
    if rel.is_empty() || rel.starts_with('/') {
        bail!("invalid relative path: {rel}");
    }
    if rel.split('/').any(|c| c == "..") {
        bail!("path traversal in relative path: {rel}");
    }
    Ok(())
}

/// 上传单个文件。返回 (status, written_bytes, sha256)。
async fn upload_one(
    conn: &quinn::Connection,
    local: &Path,
    remote: &str,
) -> Result<(u8, u64, [u8; 32])> {
    let meta = tokio::fs::metadata(local)
        .await
        .with_context(|| format!("stat {}", local.display()))?;
    if meta.is_dir() {
        bail!("{} is a directory", local.display());
    }
    let file_size = meta.len();

    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[STREAM_UPLOAD, MODE_OVERWRITE]).await?;
    send.write_all(&(remote.len() as u16).to_le_bytes()).await?;
    send.write_all(remote.as_bytes()).await?;
    send.write_all(&file_size.to_le_bytes()).await?;

    let mut file = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("open {}", local.display()))?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        send.write_all(&buf[..n]).await?;
    }
    send.finish()?;

    let mut status = [0u8; 1];
    recv.read_exact(&mut status).await?;
    match status[0] {
        0x00 => {
            let mut size_buf = [0u8; 8];
            recv.read_exact(&mut size_buf).await?;
            let total = u64::from_le_bytes(size_buf);
            let mut hash = [0u8; 32];
            recv.read_exact(&mut hash).await?;
            Ok((0x00, total, hash))
        }
        0x01 => Ok((0x01, 0, [0u8; 32])),
        0x02 => {
            let mut len_buf = [0u8; 2];
            recv.read_exact(&mut len_buf).await?;
            let len = u16::from_le_bytes(len_buf) as usize;
            let mut msg = vec![0u8; len];
            recv.read_exact(&mut msg).await?;
            bail!("upload failed: {}", String::from_utf8_lossy(&msg));
        }
        c => bail!("unexpected upload status: 0x{c:02x}"),
    }
}

/// 上传文件或目录。目录时逐文件并发上传，任一失败最终返回 Err。
pub async fn upload(
    conn: &quinn::Connection,
    local_path: &str,
    remote_path: &str,
    exclude: &[String],
) -> Result<()> {
    let local = Path::new(local_path);
    let meta = tokio::fs::metadata(local)
        .await
        .with_context(|| format!("stat {local_path}"))?;

    if !meta.is_dir() {
        let (_, written, hash) = upload_one(conn, local, remote_path).await?;
        println!(
            "uploaded {local_path} → {remote_path} ({written} bytes, sha256:{})",
            hex::encode(hash)
        );
        return Ok(());
    }

    let mut files: Vec<(PathBuf, String)> = Vec::new();
    collect_files(local, local, remote_path, exclude, &mut files, 0).await?;
    if files.is_empty() {
        println!("no files to upload (empty directory or all excluded)");
        return Ok(());
    }
    println!(
        "uploading {} file(s) from {local_path} → {remote_path}",
        files.len()
    );

    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_UPLOAD_CONCURRENCY));
    let mut set = tokio::task::JoinSet::new();
    for (local_file, remote) in files {
        let permit = sem.clone().acquire_owned().await.expect("semaphore closed");
        let conn = conn.clone();
        set.spawn(async move {
            let _permit = permit;
            let res = upload_one(&conn, &local_file, &remote).await;
            (local_file, remote, res)
        });
    }

    let (mut ok, mut skipped, mut failed) = (0u32, 0u32, 0u32);
    let mut bytes = 0u64;
    while let Some(joined) = set.join_next().await {
        let (_local_file, remote, res) = joined.context("upload task panicked")?;
        match res {
            Ok((0x00, n, hash)) => {
                ok += 1;
                bytes += n;
                println!("  ok   {remote} ({n} bytes, sha256:{})", hex::encode(hash));
            }
            Ok((0x01, _, _)) => {
                skipped += 1;
                println!("  skip {remote} (already exists)");
            }
            Err(e) => {
                failed += 1;
                eprintln!("  FAIL {remote}: {e}");
            }
            _ => unreachable!("upload_one only returns status 0x00/0x01 or Err"),
        }
    }
    println!("upload done: {ok} ok ({bytes} bytes), {skipped} skipped, {failed} failed");
    if failed > 0 {
        bail!("{failed} file(s) failed to upload");
    }
    Ok(())
}

/// 从 recv 流精确读取 size 字节写入 path，返回 sha256。
async fn recv_to_file(recv: &mut quinn::RecvStream, path: &Path, size: u64) -> Result<[u8; 32]> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
    }
    let mut file = tokio::fs::File::create(path)
        .await
        .with_context(|| format!("create {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut remaining = size;
    while remaining > 0 {
        let want = CHUNK_SIZE.min(remaining as usize);
        let n = recv.read(&mut buf[..want]).await?.unwrap_or(0);
        if n == 0 {
            bail!(
                "stream ended early: expected {size} bytes, got {}",
                size - remaining
            );
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).await?;
        remaining -= n as u64;
    }
    file.flush().await?;
    Ok(hasher.finalize().into())
}

/// 下载远端文件或目录。local_path 为单文件时是目标文件路径，
/// 远端为目录时是目标目录（文件写入 local_path/<相对路径>）。
pub async fn download(conn: &quinn::Connection, remote_path: &str, local_path: &str) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[STREAM_DOWNLOAD]).await?;
    send.write_all(&(remote_path.len() as u16).to_le_bytes())
        .await?;
    send.write_all(remote_path.as_bytes()).await?;
    send.finish()?;

    let mut status = [0u8; 1];
    recv.read_exact(&mut status).await?;
    match status[0] {
        0x00 => {
            let mut size_buf = [0u8; 8];
            recv.read_exact(&mut size_buf).await?;
            let size = u64::from_le_bytes(size_buf);
            let hash = recv_to_file(&mut recv, Path::new(local_path), size).await?;
            println!(
                "downloaded {remote_path} → {local_path} ({size} bytes, sha256:{})",
                hex::encode(hash)
            );
            Ok(())
        }
        0x04 => {
            let mut count_buf = [0u8; 4];
            recv.read_exact(&mut count_buf).await?;
            let count = u32::from_le_bytes(count_buf);
            let base = Path::new(local_path);
            tokio::fs::create_dir_all(base)
                .await
                .with_context(|| format!("mkdir {local_path}"))?;
            let mut bytes = 0u64;
            for _ in 0..count {
                let mut len_buf = [0u8; 2];
                recv.read_exact(&mut len_buf).await?;
                let len = u16::from_le_bytes(len_buf) as usize;
                let mut rel_bytes = vec![0u8; len];
                recv.read_exact(&mut rel_bytes).await?;
                let rel = String::from_utf8(rel_bytes).context("invalid UTF-8 in rel path")?;
                validate_rel_path(&rel)?;
                let mut size_buf = [0u8; 8];
                recv.read_exact(&mut size_buf).await?;
                let size = u64::from_le_bytes(size_buf);
                let hash = recv_to_file(&mut recv, &base.join(&rel), size).await?;
                bytes += size;
                println!("  ok  {rel} ({size} bytes, sha256:{})", hex::encode(hash));
            }
            println!("downloaded {count} file(s) ({bytes} bytes) → {local_path}");
            Ok(())
        }
        0x02 => {
            let mut len_buf = [0u8; 2];
            recv.read_exact(&mut len_buf).await?;
            let len = u16::from_le_bytes(len_buf) as usize;
            let mut msg = vec![0u8; len];
            recv.read_exact(&mut msg).await?;
            bail!("download failed: {}", String::from_utf8_lossy(&msg));
        }
        c => bail!("unexpected download status: 0x{c:02x}"),
    }
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
        assert!(!should_exclude("src/main.rs", &patterns));
        assert!(!should_exclude("a.log.bak", &patterns));
    }

    #[test]
    fn test_validate_rel_path() {
        assert!(validate_rel_path("a.txt").is_ok());
        assert!(validate_rel_path("sub/dir/a.txt").is_ok());
        assert!(validate_rel_path("").is_err());
        assert!(validate_rel_path("/abs/path").is_err());
        assert!(validate_rel_path("../escape").is_err());
        assert!(validate_rel_path("sub/../../escape").is_err());
        // ".." 作为文件名片段是合法的，只有独立分量 ".." 才拒绝
        assert!(validate_rel_path("foo..bar/baz").is_ok());
    }

    #[tokio::test]
    async fn test_collect_files_recursive_with_exclude() {
        let root = TempDir::new().unwrap();
        let base = root.path();
        std::fs::write(base.join("a.txt"), b"a").unwrap();
        std::fs::create_dir_all(base.join("sub/deep")).unwrap();
        std::fs::write(base.join("sub/b.log"), b"b").unwrap();
        std::fs::write(base.join("sub/deep/c.txt"), b"c").unwrap();

        let mut files: Vec<(PathBuf, String)> = Vec::new();
        collect_files(base, base, "/remote/dir", &["*.log".into()], &mut files, 0)
            .await
            .unwrap();

        let mut remotes: Vec<String> = files.iter().map(|(_, r)| r.clone()).collect();
        remotes.sort();
        assert_eq!(
            remotes,
            vec!["/remote/dir/a.txt", "/remote/dir/sub/deep/c.txt"]
        );
    }

    #[tokio::test]
    async fn test_collect_files_trailing_slash_remote() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("a.txt"), b"a").unwrap();
        let mut files: Vec<(PathBuf, String)> = Vec::new();
        collect_files(root.path(), root.path(), "/remote/", &[], &mut files, 0)
            .await
            .unwrap();
        assert_eq!(files[0].1, "/remote/a.txt");
    }
}
