use anyhow::Context;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::transport::{connect_to_host_stream, recv_json_frame, send_json_frame};
use yunying_core::types::HostConfig;

const STREAM_FRAME_MAGIC: u8 = 0x02;
const MAX_BUFFER_SIZE: usize = 10000;
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;
const DEFAULT_KEEPALIVE_SECS: u64 = 30;

pub struct StreamManager {
    streams: Arc<Mutex<HashMap<String, Arc<StreamConnection>>>>,
}

struct StreamConnection {
    buffer: Arc<Mutex<VecDeque<String>>>,
    reader_task: JoinHandle<()>,
}

impl Drop for StreamConnection {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

/// 解析 bridge 端推送的二进制 stream 数据帧：
/// `0x02` + pane_id_len(u32 LE) + pane_id + text_len(u32 LE) + text
///
/// 返回 text 内容（忽略 pane_id，subscribe 时已绑定 pane）。
async fn read_stream_frame(stream: &mut (impl AsyncReadExt + Unpin)) -> anyhow::Result<String> {
    let mut magic = [0u8; 1];
    stream.read_exact(&mut magic).await?;
    if magic[0] != STREAM_FRAME_MAGIC {
        anyhow::bail!("unexpected stream frame magic: 0x{:02x}", magic[0]);
    }

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let pid_len = u32::from_le_bytes(len_buf) as usize;
    if pid_len > 1024 {
        anyhow::bail!("pane_id too long: {} bytes", pid_len);
    }
    let mut pid = vec![0u8; pid_len];
    stream.read_exact(&mut pid).await?;

    stream.read_exact(&mut len_buf).await?;
    let text_len = u32::from_le_bytes(len_buf) as usize;
    if text_len > 1024 * 1024 {
        anyhow::bail!("stream text too large: {} bytes", text_len);
    }
    let mut text_buf = vec![0u8; text_len];
    stream.read_exact(&mut text_buf).await?;
    Ok(String::from_utf8_lossy(&text_buf).into_owned())
}

impl StreamManager {
    pub fn new() -> Self {
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn stream_pane(
        &self,
        ctx: &crate::tools::ToolContext,
        host: &HostConfig,
        session_name: &str,
        pane_id: &str,
        timeout_ms: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let key = format!("{}:{}:{}", host.name, session_name, pane_id);

        let conn = {
            let streams = self.streams.lock().await;
            streams.get(&key).cloned()
        };

        let conn = if let Some(c) = conn {
            tracing::info!("stream_pane: reusing existing connection for key={}", key);
            c
        } else {
            tracing::info!("stream_pane: creating new connection for key={}", key);
            let mut tls = connect_to_host_stream(
                ctx,
                host,
                DEFAULT_IDLE_TIMEOUT_SECS,
                DEFAULT_KEEPALIVE_SECS,
            )
            .await
            .with_context(|| format!("failed to connect to bridge for stream: {}", host.name))?;

            tracing::info!(
                "stream_pane: connected, sending stream_subscribe for session={} pane={}",
                session_name,
                pane_id
            );
            send_json_frame(
                &mut tls,
                &json!({
                    "type": "stream_subscribe",
                    "session_name": session_name,
                    "pane_id": pane_id,
                }),
            )
            .await
            .with_context(|| "failed to send stream_subscribe")?;

            tracing::info!("stream_pane: waiting for ack");
            let ack = recv_json_frame(&mut tls)
                .await
                .with_context(|| "failed to receive stream_subscribe ack")?;
            tracing::info!("stream_pane: received ack: {:?}", ack);
            if !ack.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                let err = ack
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                anyhow::bail!("stream_subscribe failed: {}", err);
            }

            let tls = Arc::new(Mutex::new(tls));
            let buffer = Arc::new(Mutex::new(VecDeque::<String>::new()));

            let tls_clone = Arc::clone(&tls);
            let buffer_clone = Arc::clone(&buffer);
            let key_clone = key.clone();
            let streams_clone = Arc::clone(&self.streams);

            let reader_task = tokio::spawn(async move {
                loop {
                    let text = {
                        let mut guard = tls_clone.lock().await;
                        read_stream_frame(&mut *guard).await
                    };
                    match text {
                        Ok(t) => {
                            let mut buf = buffer_clone.lock().await;
                            if buf.len() < MAX_BUFFER_SIZE {
                                buf.push_back(t);
                            } else {
                                tracing::warn!(
                                    "stream buffer full ({MAX_BUFFER_SIZE}), dropping data for {key_clone}"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::debug!("stream reader for {key_clone} disconnected: {e}");
                            streams_clone.lock().await.remove(&key_clone);
                            break;
                        }
                    }
                }
            });

            let conn = Arc::new(StreamConnection {
                buffer,
                reader_task,
            });

            self.streams
                .lock()
                .await
                .insert(key.clone(), Arc::clone(&conn));
            conn
        };

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(text) = conn.buffer.lock().await.pop_front() {
                return Ok(json!({"text": text}));
            }
            if Instant::now() >= deadline {
                return Ok(json!({"text": ""}));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
