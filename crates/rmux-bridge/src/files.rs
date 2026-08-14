use std::sync::Arc;

use clum_core::COPY_BUF_SIZE;

use anyhow::Context;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::bridge_audit::BridgeAuditDb;
use crate::interactive::InteractiveSession;

/// 检查文件路径安全性：拒绝 null byte 和路径穿越（`..`）。
/// 合法路径直接放行，不做规范化 — 运维工具需要完整文件系统访问。
/// 符号链接防护在下载侧处理：目录遍历跳过符号链接（见 collect_remote_files）。
fn sanitize_path(raw: &str) -> anyhow::Result<String> {
    if raw.contains('\0') {
        anyhow::bail!("path contains null byte");
    }

    if raw.contains("..") {
        anyhow::bail!("path traversal rejected: '{}'", raw);
    }

    tracing::info!(operation = "file_access", path = raw, "file access");
    Ok(raw.to_string())
}

// ─── QUIC stream handlers ───

/// QUIC stream dispatcher: read stream type byte, route to handler.
/// 0x01 = JSON protocol frames (LE32 length prefix), 0x02 = file upload, 0x03 = file download,
/// 0x05 = port forward, 0x08 = directory listing (for parallel download).
#[allow(clippy::too_many_arguments)]
pub async fn handle_quic_stream(
    send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    protocol_proxy: std::sync::Arc<tokio::sync::RwLock<crate::protocol::ProtocolProxy>>,
    session_state: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, InteractiveSession>>,
    >,
    session_counts: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    recording_enabled: bool,
    recording_dir: std::path::PathBuf,
    fsync_interval_secs: u64,
    audit_db: Arc<BridgeAuditDb>,
    idle_timeout_secs: u64,
) -> anyhow::Result<()> {
    let mut type_buf = [0u8; 1];
    recv.read_exact(&mut type_buf).await?;
    match type_buf[0] {
        0x01 => {
            let proxy = protocol_proxy.read().await;
            let gen_before = proxy.generation();
            let adapter = crate::proxy::QuicStreamAdapter { recv, send };
            let result =
                crate::proxy::proxy_protocol_aware(adapter, &proxy, audit_db, recording_dir).await;
            if let Err(ref e) = result {
                let msg = format!("{e:#}").to_lowercase();
                if msg.contains("timed out") || msg.contains("timeout") {
                    drop(proxy);
                    let mut guard = protocol_proxy.write().await;
                    if guard.generation() == gen_before {
                        if let Err(e2) = guard.reconnect().await {
                            tracing::error!("rmux reconnect failed: {e2}");
                        }
                    }
                }
            }
            result
        }
        0x02 => handle_upload_quic(send, recv).await,
        0x03 => handle_download_quic(send, recv).await,
        0x05 => handle_forward_quic(send, recv).await,
        0x08 => handle_list_quic(send, recv).await,
        0x06 => {
            crate::interactive::handle_interactive_control(
                send,
                recv,
                protocol_proxy.clone(),
                session_state.clone(),
                audit_db,
                idle_timeout_secs,
            )
            .await
        }
        0x07 => {
            // 0x07 数据流头：1 字节 client_id_len + client_id 字节。
            // enrolled 模式多客户端共享一个 QUIC connection，必须用 client_id
            // 匹配各客户端自己的 interactive 状态。
            let mut cid_len_buf = [0u8; 1];
            recv.read_exact(&mut cid_len_buf).await?;
            let cid_len = cid_len_buf[0] as usize;
            let mut cid_buf = vec![0u8; cid_len];
            recv.read_exact(&mut cid_buf).await?;
            let client_id = String::from_utf8(cid_buf)
                .map_err(|_| anyhow::anyhow!("invalid client_id on 0x07 stream"))?;
            crate::interactive::handle_interactive_data(
                send,
                recv,
                protocol_proxy,
                session_state,
                session_counts,
                client_id,
                recording_enabled,
                recording_dir,
                fsync_interval_secs,
                audit_db,
            )
            .await
        }
        t => {
            tracing::warn!("unknown QUIC stream type: 0x{:02x}", t);
            Ok(())
        }
    }
}

async fn handle_upload_quic(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> anyhow::Result<()> {
    let mut mode_buf = [0u8; 1];
    recv.read_exact(&mut mode_buf).await?;
    let mode = mode_buf[0];

    let mut path_len_buf = [0u8; 2];
    recv.read_exact(&mut path_len_buf).await?;
    let path_len = u16::from_le_bytes(path_len_buf) as usize;
    let mut path = vec![0u8; path_len];
    recv.read_exact(&mut path).await?;
    let mut remote_path = match sanitize_path(&String::from_utf8_lossy(&path)) {
        Ok(p) => p,
        Err(e) => {
            // 发送错误信封（0x02 + 消息），与下载侧一致；直接 bail 会导致 MCP 端读到 EOF 归为 UNKNOWN
            let msg = e.to_string();
            send.write_all(&[0x02]).await?;
            let msg_len = (msg.len() as u16).to_le_bytes();
            send.write_all(&msg_len).await?;
            send.write_all(msg.as_bytes()).await?;
            send.finish()?;
            return Ok(());
        }
    };

    let mut size_buf = [0u8; 8];
    recv.read_exact(&mut size_buf).await?;
    let _declared_size = u64::from_le_bytes(size_buf);

    if let Some(parent) = std::path::Path::new(&remote_path).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    // skip mode: file exists → skip
    if mode == 0x02 && tokio::fs::metadata(&remote_path).await.is_ok() {
        send.write_all(&[0x01]).await?;
        send.write_all(&0u64.to_le_bytes()).await?;
        send.write_all(&[0u8; 32]).await?;
        send.finish()?;
        return Ok(());
    }

    // rename mode: file exists → append .1, .2, etc.
    if mode == 0x03 {
        let mut renamed = remote_path.clone();
        let mut counter = 1u32;
        while tokio::fs::metadata(&renamed).await.is_ok() {
            renamed = format!("{}.{}", remote_path, counter);
            counter += 1;
        }
        remote_path = renamed;
    }

    // no-clobber mode: file exists → error
    if mode == 0x04 && tokio::fs::metadata(&remote_path).await.is_ok() {
        send.write_all(&[0x02]).await?;
        let msg = "file already exists";
        send.write_all(&(msg.len() as u16).to_le_bytes()).await?;
        send.write_all(msg.as_bytes()).await?;
        send.finish()?;
        return Ok(());
    }

    let tmp_path = format!("{}.tmp.{}", remote_path, std::process::id());
    let mut file = tokio::fs::File::create(&tmp_path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let mut total: u64 = 0;

    loop {
        let n = match recv.read(&mut buf).await? {
            Some(0) | None => break,
            Some(n) => n,
        };
        file.write_all(&buf[..n]).await?;
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    file.flush().await?;
    drop(file);

    let hash: [u8; 32] = hasher.finalize().into();
    tokio::fs::rename(&tmp_path, &remote_path).await?;

    send.write_all(&[0x00]).await?;
    send.write_all(&total.to_le_bytes()).await?;
    send.write_all(&hash).await?;
    send.finish()?;
    tracing::info!("QUIC uploaded {} ({} bytes)", remote_path, total);
    Ok(())
}

async fn handle_download_quic(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> anyhow::Result<()> {
    let mut path_len_buf = [0u8; 2];
    recv.read_exact(&mut path_len_buf).await?;
    let path_len = u16::from_le_bytes(path_len_buf) as usize;
    let mut path = vec![0u8; path_len];
    recv.read_exact(&mut path).await?;
    let remote_path = match sanitize_path(&String::from_utf8_lossy(&path)) {
        Ok(p) => p,
        Err(e) => {
            let msg = e.to_string();
            send.write_all(&[0x02]).await?;
            let msg_len = (msg.len() as u16).to_le_bytes();
            send.write_all(&msg_len).await?;
            send.write_all(msg.as_bytes()).await?;
            send.finish()?;
            return Ok(());
        }
    };

    let meta = match tokio::fs::metadata(&remote_path).await {
        Ok(m) => m,
        Err(e) => {
            let msg = format!("failed to stat: {}", e);
            send.write_all(&[0x02]).await?;
            let msg_len = (msg.len() as u16).to_le_bytes();
            send.write_all(&msg_len).await?;
            send.write_all(msg.as_bytes()).await?;
            send.finish()?;
            return Ok(());
        }
    };

    if meta.is_dir() {
        download_dir_quic(send, &remote_path).await
    } else {
        download_file_quic(send, &remote_path).await
    }
}

async fn download_file_quic(mut send: quinn::SendStream, remote_path: &str) -> anyhow::Result<()> {
    let mut file = tokio::fs::File::open(remote_path).await?;
    let file_size = file.metadata().await?.len();

    send.write_all(&[0x00]).await?;
    send.write_all(&file_size.to_le_bytes()).await?;

    copy_with_buf(&mut file, &mut send).await?;
    send.finish()?;

    // 注意：此日志只表示文件已全部写入 bridge→server 的 QUIC 发送队列，
    // 不代表 server 已转发、更不代表 client 已收到（端到端未确认）。
    // 链路带宽不对称或 server→client 段停滞时，此日志会先于实际完成出现。
    tracing::info!(
        "QUIC download sent to server: {} ({} bytes, end-to-end unconfirmed)",
        remote_path,
        file_size
    );
    Ok(())
}

async fn download_dir_quic(mut send: quinn::SendStream, remote_path: &str) -> anyhow::Result<()> {
    let base = std::path::Path::new(remote_path);
    let mut files: Vec<(std::path::PathBuf, String)> = Vec::new();
    collect_remote_files(base, base, &mut files, 0).await?;

    send.write_all(&[0x04]).await?;
    send.write_all(&(files.len() as u32).to_le_bytes()).await?;

    for (abs_path, rel_path) in &files {
        send.write_all(&(rel_path.len() as u16).to_le_bytes())
            .await?;
        send.write_all(rel_path.as_bytes()).await?;

        let mut file = tokio::fs::File::open(abs_path).await?;
        let file_size = file.metadata().await?.len();

        send.write_all(&file_size.to_le_bytes()).await?;

        copy_with_buf(&mut file, &mut send).await?;
    }

    send.finish()?;

    // 同 download_file_quic：仅表示所有文件已写入发送队列，端到端未确认。
    tracing::info!(
        "QUIC download sent to server: {} ({} files, end-to-end unconfirmed)",
        remote_path,
        files.len()
    );
    Ok(())
}

/// 0x08 = 目录清单（并行下载用）：只返回文件列表与大小，不传内容。
/// 响应：[0x00][count u32]，随后 count × [rel_len u16][rel][size u64]；
/// 远端为单文件时 count=1 且 rel 为空串；错误：[0x02][msg_len u16][msg]。
async fn handle_list_quic(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> anyhow::Result<()> {
    let mut path_len_buf = [0u8; 2];
    recv.read_exact(&mut path_len_buf).await?;
    let path_len = u16::from_le_bytes(path_len_buf) as usize;
    let mut path = vec![0u8; path_len];
    recv.read_exact(&mut path).await?;
    let remote_path = sanitize_path(&String::from_utf8_lossy(&path))?;

    let meta = match tokio::fs::metadata(&remote_path).await {
        Ok(m) => m,
        Err(e) => {
            let msg = format!("failed to stat: {e}");
            send.write_all(&[0x02]).await?;
            send.write_all(&(msg.len() as u16).to_le_bytes()).await?;
            send.write_all(msg.as_bytes()).await?;
            send.finish()?;
            return Ok(());
        }
    };

    let entries: Vec<(String, u64)> = if meta.is_dir() {
        let base = std::path::Path::new(&remote_path);
        let mut files: Vec<(std::path::PathBuf, String)> = Vec::new();
        collect_remote_files(base, base, &mut files, 0).await?;
        let mut out = Vec::with_capacity(files.len());
        for (abs_path, rel) in files {
            let size = tokio::fs::metadata(&abs_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            out.push((rel, size));
        }
        out
    } else {
        vec![(String::new(), meta.len())]
    };

    send.write_all(&[0x00]).await?;
    send.write_all(&(entries.len() as u32).to_le_bytes())
        .await?;
    for (rel, size) in &entries {
        send.write_all(&(rel.len() as u16).to_le_bytes()).await?;
        send.write_all(rel.as_bytes()).await?;
        send.write_all(&size.to_le_bytes()).await?;
    }
    send.finish()?;

    tracing::info!("QUIC listed {} ({} files)", remote_path, entries.len());
    Ok(())
}

async fn collect_remote_files(
    base: &std::path::Path,
    dir: &std::path::Path,
    files: &mut Vec<(std::path::PathBuf, String)>,
    depth: u32,
) -> anyhow::Result<()> {
    if depth > 64 {
        anyhow::bail!("directory too deep (>64): {}", dir.display());
    }
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        // 用 symlink_metadata 避免跟随符号链接：目录里的符号链接可能指向
        // 下载范围之外（如 secret -> /root/.ssh），跟随会把范围外文件带出。
        let meta = match tokio::fs::symlink_metadata(&path).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            tracing::debug!("download: skipping symlink {}", path.display());
            continue;
        }
        if meta.is_dir() {
            Box::pin(collect_remote_files(base, &path, files, depth + 1)).await?;
        } else {
            let rel = path.strip_prefix(base)?.to_string_lossy().to_string();
            files.push((path, rel));
        }
    }
    Ok(())
}

async fn handle_forward_quic(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> anyhow::Result<()> {
    let mut host_len_buf = [0u8; 2];
    recv.read_exact(&mut host_len_buf).await?;
    let host_len = u16::from_le_bytes(host_len_buf) as usize;

    if host_len > 253 {
        anyhow::bail!("host name too long: {} (max 253)", host_len);
    }

    let mut host_buf = vec![0u8; host_len];
    recv.read_exact(&mut host_buf).await?;
    let remote_host = String::from_utf8_lossy(&host_buf).to_string();

    let mut port_buf = [0u8; 2];
    recv.read_exact(&mut port_buf).await?;
    let remote_port = u16::from_le_bytes(port_buf);

    let target = format!("{}:{}", remote_host, remote_port);
    let tcp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::net::TcpStream::connect(&target),
    )
    .await
    .context("TCP connect timeout")??;

    let (mut tcp_read, mut tcp_write) = tcp.into_split();

    let tcp_to_quic = async {
        let mut buf = vec![0u8; COPY_BUF_SIZE];
        loop {
            let n = tcp_read.read(&mut buf).await?;
            if n == 0 {
                send.finish()?;
                break;
            }
            send.write_all(&buf[..n]).await?;
        }
        Ok::<_, anyhow::Error>(())
    };

    let quic_to_tcp = async {
        let mut buf = vec![0u8; COPY_BUF_SIZE];
        loop {
            match recv.read(&mut buf).await? {
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

    tracing::info!("QUIC forward closed: {}", target);
    Ok(())
}

async fn copy_with_buf(
    reader: &mut (impl AsyncReadExt + Unpin),
    writer: &mut (impl AsyncWriteExt + Unpin),
) -> std::io::Result<u64> {
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).await?;
        total += n as u64;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_path_rejects_null_byte() {
        assert!(sanitize_path("/tmp/foo\0bar").is_err());
    }

    #[test]
    fn test_sanitize_path_rejects_dotdot() {
        assert!(sanitize_path("/tmp/../etc/passwd").is_err());
        assert!(sanitize_path("../etc/shadow").is_err());
        assert!(sanitize_path("/tmp/../../../etc/passwd").is_err());
        assert!(sanitize_path("foo/..").is_err());
    }

    #[test]
    fn test_sanitize_path_absolute() {
        let result = sanitize_path("/tmp").unwrap();
        assert_eq!(result, "/tmp");
    }

    #[test]
    fn test_sanitize_path_nonexistent_file() {
        let result = sanitize_path("/tmp/nonexistent-file-xyz-123.txt").unwrap();
        assert_eq!(result, "/tmp/nonexistent-file-xyz-123.txt");
    }

    // ── sanitize_path edge cases ──

    #[test]
    fn test_sanitize_path_empty_string() {
        let result = sanitize_path("").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_sanitize_path_bare_dotdot() {
        assert!(sanitize_path("..").is_err());
    }

    #[test]
    fn test_sanitize_path_double_dot_in_filename() {
        // "file..name" contains ".." but is NOT a path component —
        // current simple substring check rejects it as a tradeoff.
        assert!(sanitize_path("file..name").is_err());
    }

    // ── copy_with_buf chunk boundary tests ──

    #[tokio::test]
    async fn test_copy_with_buf_empty_reader() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("empty.dat");
        let dst = dir.path().join("out.dat");

        tokio::fs::File::create(&src).await.unwrap();

        let mut reader = tokio::fs::File::open(&src).await.unwrap();
        let mut writer = tokio::fs::File::create(&dst).await.unwrap();

        let total = copy_with_buf(&mut reader, &mut writer).await.unwrap();
        drop(writer);

        assert_eq!(total, 0);
        assert_eq!(std::fs::metadata(&dst).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_copy_with_buf_smaller_than_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("small.bin");
        let dst = dir.path().join("out.bin");

        let data = vec![0xAB; 4096]; // 4 KB ≪ COPY_BUF_SIZE (1 MB)
        tokio::fs::write(&src, &data).await.unwrap();

        let mut reader = tokio::fs::File::open(&src).await.unwrap();
        let mut writer = tokio::fs::File::create(&dst).await.unwrap();

        let total = copy_with_buf(&mut reader, &mut writer).await.unwrap();
        drop(writer);

        assert_eq!(total, data.len() as u64);
        assert_eq!(std::fs::read(&dst).unwrap(), data);
    }

    #[tokio::test]
    async fn test_copy_with_buf_exact_chunk_size() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("exact.bin");
        let dst = dir.path().join("out.bin");

        let data = vec![0xCD; COPY_BUF_SIZE]; // exactly 1 MB
        tokio::fs::write(&src, &data).await.unwrap();

        let mut reader = tokio::fs::File::open(&src).await.unwrap();
        let mut writer = tokio::fs::File::create(&dst).await.unwrap();

        let total = copy_with_buf(&mut reader, &mut writer).await.unwrap();
        drop(writer);

        assert_eq!(total, COPY_BUF_SIZE as u64);
        assert_eq!(std::fs::read(&dst).unwrap(), data);
    }
}
