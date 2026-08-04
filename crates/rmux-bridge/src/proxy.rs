//! Protocol-aware TLS proxy: receives framed JSON requests from the MCP server
//! over a TLS stream, dispatches them to the `ProtocolProxy`, and returns
//! framed JSON responses. Handles streaming uploads/downloads inline.

use anyhow::Result;
use serde_json::json;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::bridge_audit::BridgeAuditDb;
use crate::protocol::ProtocolProxy;
use clum_core::MAX_FRAME_SIZE;

/// Main event loop: reads length-prefixed JSON frames from `tls_stream`,
/// dispatches each request to `protocol_proxy`, and writes back the response.
/// Special handling for file upload/download streaming frames.
pub async fn proxy_protocol_aware<S>(
    tls_stream: S,
    protocol_proxy: &ProtocolProxy,
    audit_db: Arc<BridgeAuditDb>,
    recording_dir: std::path::PathBuf,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, writer) = tokio::io::split(tls_stream);
    let writer = Arc::new(tokio::sync::Mutex::new(writer));

    let mut len_buf = [0u8; 4];
    let mut handled = false;

    loop {
        if let Err(e) = reader.read_exact(&mut len_buf).await {
            tracing::debug!(
                "client disconnected after {} request(s): {}",
                handled as u32,
                e
            );
            break;
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_FRAME_SIZE {
            let err = json!({"ok": false, "error": format!("frame too large: {} bytes (max {})", len, MAX_FRAME_SIZE)});
            send_response(&writer, &err).await?;
            continue;
        }

        let mut buf = vec![0u8; len];
        if let Err(e) = reader.read_exact(&mut buf).await {
            if handled {
                tracing::debug!(
                    "client disconnected during frame body read after {} request(s)",
                    handled as u32
                );
            } else {
                tracing::warn!("frame read body error: {e}");
            }
            break;
        }

        let request: serde_json::Value = match serde_json::from_slice(&buf) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = json!({"error": format!("invalid json: {}", e)});
                send_response(&writer, &err_resp).await?;
                continue;
            }
        };

        let req_type = request["type"].as_str().unwrap_or("");
        let session_name = request["session_name"].as_str().unwrap_or("");
        let pane_id = request["pane_id"].as_str().unwrap_or("");

        tracing::debug!(
            type = req_type,
            session = session_name,
            pane = pane_id,
            request = %request,
            "request received"
        );

        let start = std::time::Instant::now();

        // audit_query: handle locally via BridgeAuditDb (not forwarded to rmux)
        if request["command"].as_str() == Some("audit_query") {
            let params = &request["params"];
            let event_type = params["event_type"].as_str();
            let session_name = params["session_name"].as_str();
            let since = params["since"].as_str();
            let until = params["until"].as_str();
            let limit = (params["limit"].as_u64().unwrap_or(50) as usize).min(10_000);

            let response = match audit_db
                .query(event_type, session_name, since, until, limit)
                .await
            {
                Ok(events) => json!({"events": events}),
                Err(e) => json!({"error": format!("audit query failed: {}", e)}),
            };
            send_response(&writer, &response).await?;
            handled = true;
            continue;
        }

        // audit_stats: handle locally via BridgeAuditDb (not forwarded to rmux)
        if request["command"].as_str() == Some("audit_stats") {
            let params = &request["params"];
            let since = params["since"].as_str();

            let response = match audit_db.stats(since).await {
                Ok((total, events_by_type)) => {
                    json!({"total": total, "events_by_type": events_by_type})
                }
                Err(e) => json!({"error": format!("audit stats failed: {}", e)}),
            };
            send_response(&writer, &response).await?;
            handled = true;
            continue;
        }

        // list_unsynced_recordings: handle locally (not forwarded to rmux)
        if request["command"].as_str() == Some("list_unsynced_recordings") {
            let response = match crate::cast_recorder::list_unsynced(&recording_dir).await {
                Ok(files) => json!({"files": files}),
                Err(e) => json!({"error": format!("list_unsynced failed: {}", e)}),
            };
            send_response(&writer, &response).await?;
            handled = true;
            continue;
        }

        // mark_synced: handle locally (not forwarded to rmux)
        if request["command"].as_str() == Some("mark_synced") {
            let params = &request["params"];
            let file = params["file"].as_str().unwrap_or("");
            let date = params["date"].as_str().unwrap_or("");
            let response = match crate::cast_recorder::mark_synced(&recording_dir, file, date).await
            {
                Ok(()) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": format!("mark_synced failed: {}", e)}),
            };
            send_response(&writer, &response).await?;
            handled = true;
            continue;
        }

        // file_upload: DEPRECATED — use QUIC file transfer instead
        if req_type == "file_upload" {
            let err = json!({"ok": false, "error": "TCP file upload is deprecated, use QUIC file transfer instead"});
            send_response(&writer, &err).await?;
            continue;
        }

        // file_download: DEPRECATED — use QUIC file transfer instead
        if req_type == "file_download" {
            let err = json!({"ok": false, "error": "TCP file download is deprecated, use QUIC file transfer instead"});
            send_response(&writer, &err).await?;
            continue;
        }

        if req_type == "stream_subscribe" {
            tracing::info!("stream_subscribe: received request");
            let sn = request["session_name"].as_str().unwrap_or("");
            let pane_id = request["pane_id"].as_str().unwrap_or("");
            tracing::info!("stream_subscribe: session_name={}, pane_id={}", sn, pane_id);
            match protocol_proxy.subscribe_pane_output(sn, pane_id).await {
                Ok(mut out_stream) => {
                    tracing::info!("stream_subscribe: subscribe_pane_output succeeded");
                    let resp = json!({
                        "ok": true,
                        "stream_subscribed": true,
                        "pane_id": pane_id,
                    });
                    send_response(&writer, &resp).await?;
                    tracing::info!("stream_subscribe: sent ack response");

                    let writer_clone = writer.clone();
                    let pid = pane_id.to_string();
                    tokio::spawn(async move {
                        loop {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(30),
                                out_stream.rx.recv(),
                            )
                            .await
                            {
                                Ok(Some(text)) => {
                                    let mut w = writer_clone.lock().await;
                                    if w.write_all(&[0x02]).await.is_err() {
                                        break;
                                    }
                                    if w.write_all(&(pid.len() as u32).to_le_bytes())
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                    if w.write_all(pid.as_bytes()).await.is_err() {
                                        break;
                                    }
                                    if w.write_all(&(text.len() as u32).to_le_bytes())
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                    if w.write_all(text.as_bytes()).await.is_err() {
                                        break;
                                    }
                                }
                                Ok(None) => break,
                                Err(_) => break,
                            }
                        }
                    });
                }
                Err(e) => {
                    let err_resp = json!({"ok": false, "error": e.to_string()});
                    send_response(&writer, &err_resp).await?;
                }
            }
            handled = true;
            continue;
        }

        let response = match req_type {
            "new_session" => {
                let name = request["name"].as_str().unwrap_or("clum");
                let detached = request["detached"].as_bool().unwrap_or(true);
                protocol_proxy.handle_new_session(name, detached).await
            }
            "list_sessions" => protocol_proxy.handle_list_sessions().await,
            "attach_session" => {
                let name = request["session_name"].as_str().unwrap_or("");
                protocol_proxy.handle_attach_session(name).await
            }
            "detach_session" => {
                let name = request["session_name"].as_str().unwrap_or("");
                protocol_proxy.handle_detach_session(name).await
            }
            "send_keys" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let keys = request["keys"].as_str().unwrap_or("");
                protocol_proxy.handle_send_keys(sn, pane_id, keys).await
            }
            "capture_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let max_lines = request["max_lines"].as_u64().map(|v| v as usize);
                let ansi = request["ansi"].as_bool();
                let start_line = request["start_line"].as_i64();
                let end_line = request["end_line"].as_i64();
                let join_wrapped = request["join_wrapped"].as_bool();
                let preserve_spaces = request["preserve_spaces"].as_bool();
                let alternate = request["alternate"].as_bool();
                let buffer_name = request["buffer_name"].as_str().map(String::from);
                protocol_proxy
                    .handle_capture_pane(
                        sn,
                        pane_id,
                        max_lines,
                        ansi,
                        start_line,
                        end_line,
                        join_wrapped,
                        preserve_spaces,
                        alternate,
                        buffer_name,
                    )
                    .await
            }
            "wait_for_text" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let text = request["text"].as_str().unwrap_or("");
                let timeout = request["timeout_ms"].as_u64().unwrap_or(30000);
                protocol_proxy
                    .handle_wait_for_text(sn, pane_id, text, timeout)
                    .await
            }
            "wait_exit" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let timeout = request["timeout_ms"].as_u64().unwrap_or(30000);
                protocol_proxy.handle_wait_exit(sn, pane_id, timeout).await
            }
            "split_window" => {
                let session_name = request["session_name"].as_str().unwrap_or("");
                let dir = request["direction"].as_str().unwrap_or("horizontal");
                protocol_proxy.handle_split_window(session_name, dir).await
            }
            "spawn_command" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let cmd = request["command"].as_str().unwrap_or("");
                let args: Vec<String> = request["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                protocol_proxy
                    .handle_spawn_command(sn, pane_id, cmd, &args)
                    .await
            }
            "shell_command" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let cmd = request["command"].as_str().unwrap_or("");
                protocol_proxy.handle_shell_command(sn, pane_id, cmd).await
            }
            "respawn_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let command = request["command"].as_str().map(String::from);
                let args: Option<Vec<String>> = request["args"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });
                let shell = request["shell"].as_bool();
                let cwd = request["cwd"].as_str().map(String::from);
                let env = request.get("env").cloned();
                let kill = request["kill"].as_bool();
                let keep_alive_on_exit = request["keep_alive_on_exit"].as_bool();
                protocol_proxy
                    .handle_respawn_pane(
                        sn,
                        pane_id,
                        command,
                        args,
                        shell,
                        cwd,
                        env,
                        kill,
                        keep_alive_on_exit,
                    )
                    .await
            }
            "broadcast_keys" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_ids: Vec<String> = request["pane_ids"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let keys = request["keys"].as_str().unwrap_or("");
                protocol_proxy
                    .handle_broadcast_keys(sn, &pane_ids, keys)
                    .await
            }
            "cmd_escape" => {
                let args: Vec<String> = request["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                protocol_proxy.handle_cmd_escape(&args).await
            }
            "split_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let dir = request["direction"].as_str().unwrap_or("horizontal");
                protocol_proxy.handle_split_pane(sn, pane_id, dir).await
            }
            "resize_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let cols = u16::try_from(request["cols"].as_u64().unwrap_or(80))
                    .map_err(|_| anyhow::anyhow!("cols must be 0-65535"))?;
                let rows = u16::try_from(request["rows"].as_u64().unwrap_or(24))
                    .map_err(|_| anyhow::anyhow!("rows must be 0-65535"))?;
                protocol_proxy
                    .handle_resize_pane(sn, pane_id, cols, rows)
                    .await
            }
            "send_text" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let text = request["text"].as_str().unwrap_or("");
                protocol_proxy.handle_send_text(sn, pane_id, text).await
            }
            "set_pane_title" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let title = request["title"].as_str().unwrap_or("");
                protocol_proxy
                    .handle_set_pane_title(sn, pane_id, title)
                    .await
            }
            "find_pane_text" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let pattern = request["pattern"].as_str().unwrap_or("");
                protocol_proxy
                    .handle_find_pane_text(sn, pane_id, pattern)
                    .await
            }
            "close_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                protocol_proxy.handle_close_pane(sn, pane_id).await
            }
            "close_window" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                protocol_proxy.handle_close_window(sn, idx).await
            }
            "kill_session" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                protocol_proxy.handle_kill_session(sn).await
            }
            "rename_window" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                let name = request["name"].as_str().unwrap_or("");
                protocol_proxy.handle_rename_window(sn, idx, name).await
            }
            "list_window_panes" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                protocol_proxy.handle_list_window_panes(sn, idx).await
            }
            "resize_window" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                let w = request["width"]
                    .as_u64()
                    .map(|v| u16::try_from(v).map_err(|_| anyhow::anyhow!("width must be 0-65535")))
                    .transpose()?;
                let h = request["height"]
                    .as_u64()
                    .map(|v| {
                        u16::try_from(v).map_err(|_| anyhow::anyhow!("height must be 0-65535"))
                    })
                    .transpose()?;
                protocol_proxy.handle_resize_window(sn, idx, w, h).await
            }
            "select_window" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                protocol_proxy.handle_select_window(sn, idx).await
            }
            "select_layout" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                let layout = request["layout"].as_str().unwrap_or("tiled");
                protocol_proxy.handle_select_layout(sn, idx, layout).await
            }
            "pane_info" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                protocol_proxy.handle_pane_info(sn, pane_id).await
            }
            "window_info" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let idx = request["window_index"].as_u64().unwrap_or(0) as u32;
                protocol_proxy.handle_window_info(sn, idx).await
            }
            "pane_exists" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                protocol_proxy.handle_pane_exists(sn, pane_id).await
            }
            "find_panes" => protocol_proxy.handle_find_panes(&request).await,
            "find_sessions" => protocol_proxy.handle_find_sessions(&request).await,
            "get_pane_title" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                protocol_proxy.handle_get_pane_title(sn, pane_id).await
            }
            "find_text_all" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let pattern = request["pattern"].as_str().unwrap_or("");
                protocol_proxy
                    .handle_find_text_all(sn, pane_id, pattern)
                    .await
            }
            "clear_history" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                protocol_proxy.handle_clear_history(sn, pane_id).await
            }
            "list_buffers" => protocol_proxy.handle_list_buffers().await,
            "paste_buffer" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let buffer_name = request["buffer_name"].as_str().unwrap_or("");
                protocol_proxy
                    .handle_paste_buffer(sn, pane_id, buffer_name)
                    .await
            }
            "delete_buffer" => {
                let buffer_name = request["buffer_name"].as_str().unwrap_or("");
                protocol_proxy.handle_delete_buffer(buffer_name).await
            }
            "split_pane_with" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let direction = request["direction"].as_str().unwrap_or("horizontal");
                let command = request["command"].as_str().unwrap_or("");
                let args: Vec<String> = request["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let shell = request["shell"].as_bool().unwrap_or(true);
                let cwd = request["cwd"].as_str().map(String::from);
                let env = request.get("env").cloned();
                let title = request["title"].as_str().map(String::from);
                let keep_alive_on_exit = request["keep_alive_on_exit"].as_bool();
                protocol_proxy
                    .handle_split_pane_with(
                        sn,
                        pane_id,
                        direction,
                        command,
                        &args,
                        shell,
                        cwd,
                        env,
                        title,
                        keep_alive_on_exit,
                    )
                    .await
            }
            "get_pane_by_title" => {
                let title = request["title"].as_str().unwrap_or("");
                protocol_proxy.handle_get_pane_by_title(title).await
            }
            "collect_until_exit" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let max_bytes = request["max_bytes"].as_u64().unwrap_or(1048576) as usize;
                let timeout_ms = request["timeout_ms"].as_u64().unwrap_or(60000);
                let starting_at = request["starting_at"].as_str().unwrap_or("now");
                protocol_proxy
                    .handle_collect_until_exit(sn, pane_id, max_bytes, timeout_ms, starting_at)
                    .await
            }
            "break_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let destination_window = request["destination_window"].as_u64().map(|v| v as u32);
                let detached = request["detached"].as_bool().unwrap_or(false);
                protocol_proxy
                    .handle_break_pane(sn, pane_id, destination_window, detached)
                    .await
            }
            "join_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let source_pane_id = request["source_pane_id"].as_str().unwrap_or("");
                let target_pane_id = request["target_pane_id"].as_str().unwrap_or("");
                let direction = request["direction"].as_str();
                let size = request["size"].as_u64().map(|v| v as u32);
                protocol_proxy
                    .handle_join_pane(sn, source_pane_id, target_pane_id, direction, size)
                    .await
            }
            "swap_pane" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let source_pane_id = request["source_pane_id"].as_str().unwrap_or("");
                let target_pane_id = request["target_pane_id"].as_str().unwrap_or("");
                let detached = request["detached"].as_bool().unwrap_or(false);
                protocol_proxy
                    .handle_swap_pane(sn, source_pane_id, target_pane_id, detached)
                    .await
            }
            "capabilities" => {
                let check = request["check"].as_str();
                protocol_proxy.handle_capabilities(check).await
            }
            "capture_region" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let row = request["row"].as_u64().and_then(|v| u16::try_from(v).ok());
                let col = request["col"].as_u64().and_then(|v| u16::try_from(v).ok());
                let rows = request["rows"].as_u64().and_then(|v| u16::try_from(v).ok());
                let cols = request["cols"].as_u64().and_then(|v| u16::try_from(v).ok());
                let styled = request["styled"].as_bool().unwrap_or(false);
                protocol_proxy
                    .handle_capture_region(sn, pane_id, row, col, rows, cols, styled)
                    .await
            }
            "wait_for_bytes" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let bytes_b64 = request["bytes"].as_str().unwrap_or("");
                let only_new = request["only_new"].as_bool().unwrap_or(false);
                let timeout_ms = request["timeout_ms"].as_u64().unwrap_or(30000);
                protocol_proxy
                    .handle_wait_for_bytes(sn, pane_id, bytes_b64, only_new, timeout_ms)
                    .await
            }
            "wait_stable" => {
                let sn = request["session_name"].as_str().unwrap_or("");
                let pane_id = request["pane_id"].as_str().unwrap_or("");
                let stable_ms = request["stable_ms"].as_u64().unwrap_or(500);
                let timeout_ms = request["timeout_ms"].as_u64().unwrap_or(30000);
                protocol_proxy
                    .handle_wait_stable(sn, pane_id, stable_ms, timeout_ms)
                    .await
            }
            _ => json!({"error": format!("unknown request type: {}", req_type)}),
        };

        let elapsed = start.elapsed();
        let ok = response["ok"].as_bool().unwrap_or_else(|| {
            !response
                .as_object()
                .is_some_and(|o| o.contains_key("error"))
        });

        tracing::info!(
            type = req_type,
            session = session_name,
            pane = pane_id,
            duration_ms = elapsed.as_millis() as u64,
            ok = ok,
            "request completed"
        );
        tracing::debug!(
            type = req_type,
            response = %response,
            "response sent"
        );

        send_response(&writer, &response).await?;
        handled = true;
    }

    Ok(())
}

async fn send_response(
    writer: &Arc<tokio::sync::Mutex<impl AsyncWriteExt + Unpin>>,
    response: &serde_json::Value,
) -> Result<()> {
    let resp_json = serde_json::to_string(response)?;
    let mut w = writer.lock().await;
    w.write_all(&(resp_json.len() as u32).to_le_bytes()).await?;
    w.write_all(resp_json.as_bytes()).await?;
    Ok(())
}

pub struct QuicStreamAdapter {
    pub recv: quinn::RecvStream,
    pub send: quinn::SendStream,
}

impl AsyncRead for QuicStreamAdapter {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for QuicStreamAdapter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match Pin::new(&mut self.send).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => {
                Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e)))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}
