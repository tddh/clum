use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use crate::audit;
use crate::forward::ForwardManager;
use crate::recording_sync;
use crate::router::HostRouter;
use crate::stream::StreamManager;

mod batch;
mod bridge_audit;
mod buffer;
mod common;
mod deploy;
mod discovery;
mod exec;
mod file;
mod forward;
mod output;
mod pane;
mod search;
mod session;
mod window;

// Re-export audit for all sub-modules
pub(crate) use deploy::audit;

pub struct ToolContext {
    pub router: Arc<HostRouter>,
    pub ca_cert_path: Option<String>,
    pub audit_db: Arc<audit::AuditDb>,
    pub agent_name: Arc<std::sync::Mutex<String>>,
    pub caller_group: Arc<std::sync::Mutex<Option<String>>>,
    pub current_op: Arc<std::sync::Mutex<Option<String>>>,
    pub forward_manager: Arc<ForwardManager>,
    pub stream_manager: Arc<StreamManager>,
    pub recordings_dir: PathBuf,
    #[allow(dead_code)]
    pub bridge_registry: Arc<crate::registry::BridgeRegistry>,
    pub bridge_store: Arc<crate::bridge_store::BridgeStore>,
    pub file_transfer: crate::server_config::FileTransferConfig,
}

impl Clone for ToolContext {
    fn clone(&self) -> Self {
        Self {
            router: Arc::clone(&self.router),
            ca_cert_path: self.ca_cert_path.clone(),
            audit_db: Arc::clone(&self.audit_db),
            agent_name: Arc::new(std::sync::Mutex::new(
                self.agent_name
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            )),
            caller_group: Arc::new(std::sync::Mutex::new(
                self.caller_group
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            )),
            current_op: Arc::new(std::sync::Mutex::new(
                self.current_op
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            )),
            forward_manager: Arc::clone(&self.forward_manager),
            stream_manager: Arc::clone(&self.stream_manager),
            recordings_dir: self.recordings_dir.clone(),
            bridge_registry: Arc::clone(&self.bridge_registry),
            bridge_store: Arc::clone(&self.bridge_store),
            file_transfer: self.file_transfer.clone(),
        }
    }
}

async fn resolve_host_group(ctx: &ToolContext, host: &str) -> Option<String> {
    let meta_list = ctx.bridge_store.get_all_host_meta().await;
    if let Some(meta) = meta_list.iter().find(|m| m.hostname == host) {
        if !meta.group.is_empty() {
            return Some(meta.group.clone());
        }
    }
    ctx.router
        .get(host)
        .map(|h| h.group)
        .filter(|g| !g.is_empty())
}

/// All hosts whose effective group equals `group`. Effective group is the
/// registration-DB group when set, falling back to hosts.yaml — the same
/// semantics as `resolve_host_group`, so access checks and listings agree.
async fn hosts_in_group(ctx: &ToolContext, group: &str) -> std::collections::HashSet<String> {
    let mut effective: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for h in ctx.router.list() {
        if !h.group.is_empty() {
            effective.insert(h.name.clone(), h.group.clone());
        }
    }
    for m in ctx.bridge_store.get_all_host_meta().await {
        if !m.group.is_empty() {
            effective.insert(m.hostname.clone(), m.group.clone());
        }
    }
    effective
        .into_iter()
        .filter(|(_, g)| g == group)
        .map(|(h, _)| h)
        .collect()
}

async fn authorize(ctx: &ToolContext, tool_name: &str, args: &Value) -> Result<()> {
    let caller_group = ctx
        .caller_group
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    let Some(cg) = caller_group else {
        return Ok(());
    };

    if matches!(tool_name, "reload_config" | "host_set_meta") {
        anyhow::bail!("forbidden: '{tool_name}' requires superadmin (no group)");
    }

    if tool_name == "clum_usage_rules" {
        return Ok(());
    }

    if let Some(host) = args.get("host").and_then(|v| v.as_str()) {
        let host_group = resolve_host_group(ctx, host).await;
        if host_group.as_deref() != Some(cg.as_str()) {
            anyhow::bail!(
                "host '{host}' is not in your group '{cg}' (it belongs to '{}')",
                host_group.as_deref().unwrap_or("(ungrouped)")
            );
        }
    }

    if let Some(hosts) = args.get("hosts").and_then(|v| v.as_array()) {
        for h in hosts {
            if let Some(host) = h.as_str() {
                let host_group = resolve_host_group(ctx, host).await;
                if host_group.as_deref() != Some(cg.as_str()) {
                    anyhow::bail!(
                        "host '{host}' is not in your group '{cg}' (it belongs to '{}')",
                        host_group.as_deref().unwrap_or("(ungrouped)")
                    );
                }
            }
        }
    }

    Ok(())
}

pub async fn execute_tool(
    ctx: &ToolContext,
    tool_name: &str,
    args: Value,
    progress: &mut crate::progress::ProgressReporter,
) -> Result<Value> {
    authorize(ctx, tool_name, &args).await?;

    let op = uuid::Uuid::now_v7().to_string();
    *ctx.current_op.lock().unwrap_or_else(|e| e.into_inner()) = Some(op.clone());
    let start = std::time::Instant::now();

    let result = match tool_name {
        "clum_usage_rules" => Ok(json!({})),
        "host_list" => discovery::host_list(ctx).await,
        "host_filter" => discovery::host_filter(ctx, args).await,
        "host_set_meta" => discovery::host_set_meta(ctx, args).await,
        "session_create" => session::session_create(ctx, args).await,
        "session_list" => session::session_list(ctx, args).await,
        "session_attach" => session::session_attach(ctx, args).await,
        "session_detach" => session::session_detach(ctx, args).await,
        "send_keys" => pane::send_keys(ctx, args).await,
        "capture_pane" => pane::capture_pane(ctx, args).await,
        "wait_for_text" => output::wait_for_text(ctx, args).await,
        "wait_exit" => output::wait_exit(ctx, args).await,
        "shell_command" => exec::shell_command(ctx, args).await,
        "respawn_pane" => session::respawn_pane(ctx, args).await,
        "broadcast_keys" => exec::broadcast_keys(ctx, args).await,
        "cmd_escape" => exec::cmd_escape(ctx, args).await,
        "split_window" => window::split_window(ctx, args).await,
        "stream_pane" => window::stream_pane(ctx, args).await,
        "file_upload" => file::file_upload(ctx, args, progress).await,
        "file_download" => file::file_download(ctx, args, progress).await,
        "exec" => exec::exec(ctx, args, progress).await,
        "close_pane" => pane::close_pane(ctx, args).await,
        "split_pane" => pane::split_pane(ctx, args).await,
        "resize_pane" => pane::resize_pane(ctx, args).await,
        "send_text" => pane::send_text(ctx, args).await,
        "set_pane_title" => pane::set_pane_title(ctx, args).await,
        "find_pane_text" => pane::find_pane_text(ctx, args).await,
        "close_window" => window::close_window(ctx, args).await,
        "kill_session" => session::kill_session(ctx, args).await,
        "rename_window" => window::rename_window(ctx, args).await,
        "list_window_panes" => window::list_window_panes(ctx, args).await,
        "resize_window" => window::resize_window(ctx, args).await,
        "select_window" => window::select_window(ctx, args).await,
        "select_layout" => window::select_layout(ctx, args).await,
        "pane_info" => window::pane_info(ctx, args).await,
        "window_info" => window::window_info(ctx, args).await,
        "pane_exists" => window::pane_exists(ctx, args).await,
        "batch_exec" => batch::batch_exec(ctx, args, progress).await,
        "batch_upload" => batch::batch_upload(ctx, args, progress).await,
        "batch_download" => batch::batch_download(ctx, args, progress).await,
        "batch_send_keys" => batch::batch_send_keys(ctx, args, progress).await,
        "forward_create" => forward::forward_create(ctx, args).await,
        "forward_list" => forward::forward_list(ctx).await,
        "forward_close" => forward::forward_close(ctx, args).await,
        "find_panes" => discovery::find_panes(ctx, args).await,
        "find_sessions" => discovery::find_sessions(ctx, args).await,
        "get_pane_title" => pane::get_pane_title(ctx, args).await,
        "find_text_all" => output::find_text_all(ctx, args).await,
        "clear_history" => pane::clear_history(ctx, args).await,
        "list_buffers" => buffer::list_buffers(ctx, args).await,
        "paste_buffer" => buffer::paste_buffer(ctx, args).await,
        "delete_buffer" => buffer::delete_buffer(ctx, args).await,
        "split_pane_with" => pane::split_pane_with(ctx, args).await,
        "get_pane_by_title" => pane::get_pane_by_title(ctx, args).await,
        "collect_until_exit" => exec::collect_until_exit(ctx, args).await,
        "break_pane" => pane::break_pane(ctx, args).await,
        "join_pane" => pane::join_pane(ctx, args).await,
        "swap_pane" => pane::swap_pane(ctx, args).await,
        "host_capabilities" => discovery::host_capabilities(ctx, args).await,
        "capture_region" => pane::capture_region(ctx, args).await,
        "wait_for_bytes" => output::wait_for_bytes(ctx, args).await,
        "wait_stable" => output::wait_stable(ctx, args).await,
        "deploy_bridge" => deploy::deploy_bridge(ctx, args, progress).await,
        "reload_config" => session::reload_config(ctx).await,
        "query_bridge_audit" => {
            let start = std::time::Instant::now();
            let host = args["host"].as_str().unwrap_or("").to_string();
            let result = bridge_audit::query_bridge_audit(ctx, args).await;
            let duration_ms = start.elapsed().as_millis() as u64;
            match &result {
                Ok(value) => {
                    let has_error = value.get("error").and_then(|v| v.as_str()).is_some();
                    audit(
                        ctx,
                        clum_core::types::AuditAction::BridgeAuditQuery,
                        &host,
                        "",
                        None,
                        "query_bridge_audit",
                        None,
                        !has_error,
                        duration_ms,
                        value.get("error").and_then(|v| v.as_str()),
                    )
                    .await;
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    audit(
                        ctx,
                        clum_core::types::AuditAction::BridgeAuditQuery,
                        &host,
                        "",
                        None,
                        "query_bridge_audit",
                        None,
                        false,
                        duration_ms,
                        Some(&err_msg),
                    )
                    .await;
                }
            }
            result
        }
        "audit_query" => {
            let caller_group = ctx
                .caller_group
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let host_names = match caller_group {
                Some(cg) => {
                    let names: Vec<String> = hosts_in_group(ctx, &cg).await.into_iter().collect();
                    if names.is_empty() {
                        // No hosts in this group — return nothing; an empty
                        // filter would otherwise match every event.
                        return Ok(json!([]));
                    }
                    Some(names)
                }
                None => None,
            };
            let params = crate::audit::query::QueryParams {
                host: args.get("host").and_then(|v| v.as_str()).map(String::from),
                action: args
                    .get("action")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                agent: args.get("agent").and_then(|v| v.as_str()).map(String::from),
                since: args.get("since").and_then(|v| v.as_str()).map(String::from),
                until: args.get("until").and_then(|v| v.as_str()).map(String::from),
                success: args.get("success").and_then(|v| v.as_bool()),
                limit: args.get("limit").and_then(|v| v.as_u64()).map(|v| v as u32),
                host_names,
            };
            match ctx
                .audit_db
                .query(params, crate::audit::query::OutputFormat::Json)
                .await
            {
                Ok(json_str) => Ok(
                    serde_json::from_str(&json_str).unwrap_or(json!({"ok": true, "events": []}))
                ),
                Err(e) => Ok(json!({"ok": false, "error": format!("{e:#}")})),
            }
        }
        "list_recordings" => {
            let start = std::time::Instant::now();
            let host = args.get("host").and_then(|v| v.as_str());
            let date = args.get("date").and_then(|v| v.as_str());
            let session = args.get("session").and_then(|v| v.as_str());
            let result =
                recording_sync::list_local_recordings(&ctx.recordings_dir, host, date, session)
                    .await;
            let duration_ms = start.elapsed().as_millis() as u64;
            match result {
                Ok(mut list) => {
                    let caller_group = ctx
                        .caller_group
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    if let Some(cg) = caller_group {
                        let group_hosts = hosts_in_group(ctx, &cg).await;
                        list.retain(|r| {
                            r.get("host")
                                .and_then(|v| v.as_str())
                                .map(|h| group_hosts.contains(h))
                                .unwrap_or(false)
                        });
                    }
                    let value = json!({ "recordings": list, "count": list.len() });
                    audit(
                        ctx,
                        clum_core::types::AuditAction::AuditQuery,
                        "",
                        "",
                        None,
                        "list_recordings",
                        None,
                        true,
                        duration_ms,
                        None,
                    )
                    .await;
                    Ok(value)
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    audit(
                        ctx,
                        clum_core::types::AuditAction::AuditQuery,
                        "",
                        "",
                        None,
                        "list_recordings",
                        None,
                        false,
                        duration_ms,
                        Some(&err_msg),
                    )
                    .await;
                    Ok(json!({ "error": err_msg }))
                }
            }
        }
        "get_recording" => {
            let start = std::time::Instant::now();
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");

            let caller_group = ctx
                .caller_group
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(ref cg) = caller_group {
                // Recording paths are absolute: <recordings_dir>/<host>/...
                let host_from_path = std::path::Path::new(path)
                    .strip_prefix(&ctx.recordings_dir)
                    .ok()
                    .and_then(|rel| rel.components().next())
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .unwrap_or_default();
                let host_group = resolve_host_group(ctx, &host_from_path).await;
                if host_group.as_deref() != Some(cg.as_str()) {
                    anyhow::bail!("host '{host_from_path}' is not in your group '{cg}'");
                }
            }

            let result = read_recording_file(&ctx.recordings_dir, path).await;
            let duration_ms = start.elapsed().as_millis() as u64;
            match result {
                Ok(value) => {
                    audit(
                        ctx,
                        clum_core::types::AuditAction::AuditQuery,
                        "",
                        "",
                        None,
                        "get_recording",
                        None,
                        true,
                        duration_ms,
                        None,
                    )
                    .await;
                    Ok(value)
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    audit(
                        ctx,
                        clum_core::types::AuditAction::AuditQuery,
                        "",
                        "",
                        None,
                        "get_recording",
                        None,
                        false,
                        duration_ms,
                        Some(&err_msg),
                    )
                    .await;
                    Ok(json!({ "error": err_msg }))
                }
            }
        }
        "search_recordings" => search::search_recordings(ctx, args).await,
        _ => anyhow::bail!("unknown tool: {}", tool_name),
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let outcome = match &result {
        Ok(v) if v.get("ok").and_then(|x| x.as_bool()) == Some(false) => "error",
        Ok(_) => "ok",
        Err(_) => "error",
    };
    tracing::info!(
        op = %op,
        tool = %tool_name,
        result = %outcome,
        duration_ms,
        "tool call"
    );
    *ctx.current_op.lock().unwrap_or_else(|e| e.into_inner()) = None;
    result
}

/// Read a recording file's content, ensuring the resolved path stays within
/// `recordings_dir` (path-traversal protection).
async fn read_recording_file(recordings_dir: &std::path::Path, path: &str) -> Result<Value> {
    if path.is_empty() {
        anyhow::bail!("missing 'path'");
    }
    let requested = std::path::Path::new(path);
    if !requested.is_absolute() {
        anyhow::bail!("path must be absolute (use the path returned by list_recordings)");
    }

    let canonical_root = recordings_dir
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("recordings directory not found"))?;
    let canonical_path = requested
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("failed to resolve path: {e}"))?;
    if !canonical_path.starts_with(&canonical_root) {
        anyhow::bail!("path outside recordings directory");
    }

    let content = tokio::fs::read_to_string(&canonical_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read recording: {e}"))?;
    Ok(json!({
        "path": canonical_path.to_string_lossy(),
        "content": content,
    }))
}
