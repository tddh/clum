//! JSON 帧编解码的单一实现（长度前缀 u32 LE + UTF-8 JSON）。
//!
//! 历史上 MCP / CLI / bridge 各写一份：MCP 有 64MB 上限、bridge 有上限，
//! 而 CLI 对长度**无界**（恶意 bridge 可让客户端尝试数 GB 分配）。这里统一
//! 实现并用 [`crate::MAX_FRAME_SIZE`] 约束，调用方不再各自维护边界。

use anyhow::{bail, Result};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 写一帧：u32 LE 长度 + JSON 字节。
pub async fn write_json_frame<W>(stream: &mut W, value: &Value) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > crate::MAX_FRAME_SIZE {
        bail!(
            "frame too large: {} bytes (max {})",
            bytes.len(),
            crate::MAX_FRAME_SIZE
        );
    }
    let len = u32::try_from(bytes.len()).map_err(|_| anyhow::anyhow!("frame length overflow"))?;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// 读一帧：先读 u32 LE 长度（带上限校验），再读正文。
pub async fn read_json_frame<R>(stream: &mut R) -> Result<Value>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > crate::MAX_FRAME_SIZE {
        bail!(
            "frame too large: {} bytes (max {})",
            len,
            crate::MAX_FRAME_SIZE
        );
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip() {
        let value = serde_json::json!({"ok": true, "nested": {"n": 1}});
        let mut buf = Vec::new();
        write_json_frame(&mut buf, &value).await.unwrap();
        let decoded = read_json_frame(&mut std::io::Cursor::new(buf))
            .await
            .unwrap();
        assert_eq!(decoded, value);
    }

    #[tokio::test]
    async fn rejects_oversized_length_prefix() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        let err = read_json_frame(&mut std::io::Cursor::new(buf))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("frame too large"), "got: {err}");
    }
}
