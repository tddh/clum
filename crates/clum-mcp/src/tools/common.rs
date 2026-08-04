use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use super::ToolContext;
use crate::transport::{recv_json_frame, send_json_frame, BridgeStream};

/// 解析主机名 → HostConfig：优先查 HostRouter（hosts.yaml），找不到则查
/// BridgeRegistry 已注册的 enrolled bridge，构造最小 HostConfig。
/// 纯 enrolled 模式下 hosts.yaml 可能不包含该主机，但仍可正常操作。
pub(crate) async fn resolve_host_config(
    ctx: &ToolContext,
    host_name: &str,
) -> Result<clum_core::types::HostConfig> {
    // 1. 先查 hosts.yaml 静态配置（direct 模式）
    if let Some(h) = ctx.router.get(host_name) {
        return Ok(h);
    }
    // 2. 查 BridgeRegistry 已反向注册的 bridge（enrolled 模式）
    let enrolled = ctx.bridge_registry.list().await;
    if enrolled.iter().any(|b| b.hostname == host_name) {
        return Ok(clum_core::types::HostConfig {
            name: host_name.to_string(),
            bridge_addr: None,
            bridge_token: None,
            group: String::new(),
            tags: Vec::new(),
            labels: std::collections::HashMap::new(),
            allowed_tunnel_targets: None,
        });
    }
    anyhow::bail!("host not found: {}", host_name)
}

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
pub(crate) async fn resolve_hosts(
    ctx: &ToolContext,
    names: &[String],
) -> Vec<(String, Option<clum_core::types::HostConfig>)> {
    let mut result = Vec::with_capacity(names.len());
    for name in names {
        let h = resolve_host_config(ctx, name).await.ok();
        result.push((name.clone(), h));
    }
    result
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
