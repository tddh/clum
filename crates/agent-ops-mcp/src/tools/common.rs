use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use super::ToolContext;
use crate::transport::{recv_json_frame, send_json_frame, BridgeStream};

/// 内部 session_create（不记 audit）
pub(crate) async fn create_session_inner(
    stream: &mut BridgeStream,
    session_name: &str,
) -> Result<Value> {
    send_json_frame(
        stream,
        &json!({ "type": "new_session", "name": session_name, "detached": true }),
    )
    .await?;
    recv_json_frame(stream).await
}

/// 解析主机名列表 → (name, Option<HostConfig>)
pub(crate) fn resolve_hosts(
    ctx: &ToolContext,
    names: &[String],
) -> Vec<(String, Option<agent_ops_core::types::HostConfig>)> {
    names
        .iter()
        .map(|name| {
            let h = ctx.router.get(name);
            (name.clone(), h)
        })
        .collect()
}

/// 创建并发信号量（concurrency=0 → None，即不限制）
pub(crate) fn make_semaphore(limit: usize) -> Option<Arc<tokio::sync::Semaphore>> {
    if limit > 0 {
        Some(Arc::new(tokio::sync::Semaphore::new(limit)))
    } else {
        None
    }
}

/// 收集 JoinHandle 结果 → (results_map, success_count, failed_count)
pub(crate) async fn collect_batch_results(
    handles: Vec<tokio::task::JoinHandle<(String, Value)>>,
) -> (serde_json::Map<String, Value>, u32, u32) {
    let mut results_map = serde_json::Map::new();
    let mut success = 0u32;
    let mut failed = 0u32;
    for handle in handles {
        if let Ok((host_name, result)) = handle.await {
            if result["ok"].as_bool().unwrap_or(false) {
                success += 1;
            } else {
                failed += 1;
            }
            results_map.insert(host_name, result);
        } else {
            failed += 1;
            results_map.insert(
                "unknown".into(),
                json!({"ok": false, "error": "task cancelled"}),
            );
        }
    }
    (results_map, success, failed)
}

/// 解析 pane_id：如果调用方提供了就直接用，否则自动探测 window 0 中编号最小的 pane。
///
/// 返回 `(pane_id, auto_resolved)`，`auto_resolved = true` 表示是自动探测的。
///
/// 注意：破坏性工具（close_pane / paste_buffer / respawn_pane）不应调用此函数，
/// 它们必须要求调用方显式提供 pane_id。
pub(crate) async fn resolve_pane_id(
    stream: &mut BridgeStream,
    session_name: &str,
    provided: Option<&str>,
) -> Result<(String, bool)> {
    if let Some(id) = provided {
        return Ok((id.to_string(), false));
    }
    // 复用当前 QUIC 连接查询 pane 列表，零额外建连开销
    send_json_frame(
        stream,
        &json!({
            "type": "list_window_panes",
            "session_name": session_name,
            "window_index": 0
        }),
    )
    .await?;
    let resp = recv_json_frame(stream).await?;
    if resp["ok"].as_bool() == Some(false) {
        // 透传 bridge 原始错误（SESSION_NOT_FOUND 等）
        let code = resp["error_code"].as_str().unwrap_or("UNKNOWN");
        let msg = resp["error"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| resp["error"].to_string());
        anyhow::bail!("[{}] {}", code, msg);
    }
    let min = resp["panes"]
        .as_array()
        .context("no panes in list_window_panes response")?
        .iter()
        .filter_map(|p| p["pane_id"].as_str())
        .filter_map(|id| id.trim_start_matches('%').parse::<u32>().ok())
        .min()
        .context("no valid pane_id found in session window 0")?;
    Ok((format!("%{}", min), true))
}

/// 在工具响应中追加 pane 解析信息，让 AI 知道实际操作了哪个 pane。
///
/// - 始终写入 `resolved_pane_id`
/// - 仅当自动探测时写入 `auto_resolved: true`
pub(crate) fn enrich_pane_response(response: &mut Value, pane_id: &str, auto_resolved: bool) {
    if let Some(obj) = response.as_object_mut() {
        obj.insert("resolved_pane_id".into(), json!(pane_id));
        if auto_resolved {
            obj.insert("auto_resolved".into(), json!(true));
        }
    }
}
