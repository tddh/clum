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
            allowed_forward_targets: None,
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
        // 透传原始错误消息，由上层 enrich_error 基于共享分类器重新分类
        //（与 bridge 出口注入的 error_code 一致，两侧共用 clum_core::error_code）
        let msg = resp["error"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| resp["error"].to_string());
        anyhow::bail!("{}", msg);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    // ── make_semaphore ────────────────────────────────────────────

    #[test]
    fn test_make_semaphore_zero() {
        let result = make_semaphore(0);
        assert!(result.is_none(), "concurrency=0 should return None");
    }

    #[test]
    fn test_make_semaphore_positive() {
        let result = make_semaphore(3);
        assert!(result.is_some(), "concurrency>0 should return Some");
        let sem = result.unwrap();
        let _p1 = sem.try_acquire().expect("permit 1");
        let _p2 = sem.try_acquire().expect("permit 2");
        let _p3 = sem.try_acquire().expect("permit 3");
        assert!(
            sem.try_acquire().is_err(),
            "第 4 次获取许可应失败（只有 3 个许可）"
        );
    }

    #[test]
    fn test_make_semaphore_arc_shared() {
        // 验证返回 Arc，可以被 clone
        let result = make_semaphore(1);
        let sem = result.unwrap();
        let sem2: Arc<tokio::sync::Semaphore> = Arc::clone(&sem);
        // 获取唯一的许可
        let _p = sem2.try_acquire().expect("permit via clone");
        assert!(sem.try_acquire().is_err(), "原 Arc 也不应有剩余许可");
    }

    // ── enrich_pane_response ──────────────────────────────────────

    #[test]
    fn test_enrich_pane_response_with_ok_field() {
        let mut response = json!({"ok": true, "result": "done"});
        enrich_pane_response(&mut response, "%0", false);
        assert_eq!(response["resolved_pane_id"], json!("%0"));
        assert!(response
            .as_object()
            .unwrap()
            .contains_key("resolved_pane_id"));
    }

    #[test]
    fn test_enrich_pane_response_auto_resolved() {
        let mut response = json!({"ok": true});
        enrich_pane_response(&mut response, "%2", true);
        assert_eq!(response["resolved_pane_id"], json!("%2"));
        assert_eq!(response["auto_resolved"], json!(true));
    }

    #[test]
    fn test_enrich_pane_response_not_auto() {
        let mut response = json!({"ok": false, "error": "boom"});
        enrich_pane_response(&mut response, "%5", false);
        assert_eq!(response["resolved_pane_id"], json!("%5"));
        // auto_resolved 为 false 时不应写入该字段
        assert!(response.get("auto_resolved").is_none());
    }

    #[test]
    fn test_enrich_pane_response_non_object_no_panic() {
        // 非 Object 类型不应崩溃，也不应修改值
        let mut arr = json!([1, 2, 3]);
        enrich_pane_response(&mut arr, "%0", true);
        assert_eq!(arr, json!([1, 2, 3]));

        let mut s = json!("hello");
        enrich_pane_response(&mut s, "%1", false);
        assert_eq!(s, json!("hello"));

        let mut n = json!(null);
        enrich_pane_response(&mut n, "%2", false);
        assert_eq!(n, json!(null));
    }

    // ── collect_batch_results ─────────────────────────────────────

    #[tokio::test]
    async fn test_collect_batch_results_all_success() {
        let handles: Vec<_> = (0..3)
            .map(|i| {
                let host = format!("host{}", i);
                tokio::task::spawn(async move { (host.clone(), json!({"ok": true, "host": host})) })
            })
            .collect();

        let (results_map, success, failed) = collect_batch_results(handles).await;

        assert_eq!(success, 3, "all three should succeed");
        assert_eq!(failed, 0);
        assert_eq!(results_map.len(), 3);
        assert!(results_map.get("host1").unwrap()["ok"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_collect_batch_results_all_failed_ok_field() {
        let handles: Vec<_> = (0..2)
            .map(|i| {
                let host = format!("bad{}", i);
                tokio::task::spawn(async move {
                    (
                        host.clone(),
                        json!({"ok": false, "error": "something wrong"}),
                    )
                })
            })
            .collect();

        let (results_map, success, failed) = collect_batch_results(handles).await;

        assert_eq!(success, 0);
        assert_eq!(failed, 2, "两个 ok=false 都应计为 failed");
        assert_eq!(results_map.len(), 2);
    }

    #[tokio::test]
    async fn test_collect_batch_results_join_failed() {
        // 通过 abort 一个 spawned task 来模拟 JoinError
        let handle = tokio::task::spawn(async {
            // 不立即返回，让外面 abort 掉
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            ("never".to_string(), json!({"ok": true}))
        });
        handle.abort();

        let (results_map, success, failed) = collect_batch_results(vec![handle]).await;

        assert_eq!(success, 0);
        assert_eq!(failed, 1);
        assert_eq!(results_map.len(), 1);
        let unknown = results_map.get("unknown").unwrap();
        assert_eq!(unknown["ok"], json!(false));
        assert_eq!(unknown["error"], json!("task cancelled"));
    }

    #[tokio::test]
    async fn test_collect_batch_results_mixed() {
        let h1 =
            tokio::task::spawn(async { ("good".to_string(), json!({"ok": true, "data": 42})) });
        let h2 = tokio::task::spawn(async {
            (
                "bad".to_string(),
                json!({"ok": false, "error": "cmd failed"}),
            )
        });
        let h3 = tokio::task::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            ("never".to_string(), json!({"ok": true}))
        });
        h3.abort();

        let (results_map, success, failed) = collect_batch_results(vec![h1, h2, h3]).await;

        assert_eq!(success, 1, "只有 good 是 success");
        assert_eq!(failed, 2, "bad(ok=false) + cancelled");
        assert_eq!(results_map.len(), 3);
        assert_eq!(results_map["good"]["data"], json!(42));
        assert_eq!(results_map["bad"]["ok"], json!(false));
        assert_eq!(results_map["unknown"]["error"], json!("task cancelled"));
    }
}
