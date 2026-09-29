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
mod rules;
mod search;
mod session;
mod window;

// Re-export audit for all sub-modules
pub(crate) use deploy::{audit, audit_flagged};

/// ask 命中后待批准项的存活时间（秒）。超时即不可批准（fail-closed）。
const DEFAULT_APPROVAL_TTL_SECS: i64 = 900;

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
    pub recording_keyring: Arc<crate::recording_keyring::RecordingKeyring>,
    pub bridge_registry: Arc<crate::registry::BridgeRegistry>,
    pub bridge_store: Arc<crate::bridge_store::BridgeStore>,
    pub file_transfer: crate::server_config::FileTransferConfig,
    pub command_kb: Arc<clum_command_kb::KnowledgeBase>,
    pub rules: Arc<std::sync::RwLock<clum_command_kb::RuleSet>>,
    /// 规则文件路径（启动时解析；reload_config 时按此路径重载）
    pub rules_path: PathBuf,
    pub approvals: Arc<crate::approvals::ApprovalStore>,
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
            recording_keyring: Arc::clone(&self.recording_keyring),
            stream_manager: Arc::clone(&self.stream_manager),
            recordings_dir: self.recordings_dir.clone(),
            bridge_registry: Arc::clone(&self.bridge_registry),
            bridge_store: Arc::clone(&self.bridge_store),
            file_transfer: self.file_transfer.clone(),
            command_kb: Arc::clone(&self.command_kb),
            rules: Arc::clone(&self.rules),
            rules_path: self.rules_path.clone(),
            approvals: Arc::clone(&self.approvals),
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

/// Audit/recordings 类工具集。工具面已冻结在 68 个（clum-docs/INVARIANTS.md §13，
/// 不再扩张）；若未来解冻并新增访问审计或录制数据的工具，必须用 hosts_in_group
/// 强制 caller-group 过滤，且覆盖"不传 host 参数"的场景（历史教训：search_recordings
/// 曾遗漏导致跨组越权）。本清单是文档锚点 + debug 自检（防清单名写错），
/// 不构成运行时拦截——四个工具各自的分发分支内实现组过滤的强制。
const GROUP_SCOPED_TOOLS: &[&str] = &[
    "audit_query",
    "list_recordings",
    "get_recording",
    "search_recordings",
];

pub async fn execute_tool(
    ctx: &ToolContext,
    tool_name: &str,
    args: Value,
    progress: &mut crate::progress::ProgressReporter,
) -> Result<Value> {
    authorize(ctx, tool_name, &args).await?;

    // ── 命令规则引擎：deny / ask / allow 三态裁决（权限层之后、执行层之前）──
    let host_group = match args.get("host").and_then(|v| v.as_str()) {
        Some(h) => resolve_host_group(ctx, h).await,
        None => None,
    };
    if let Some((verdict, command, decision)) =
        rules::evaluate_call(ctx, tool_name, &args, host_group)
    {
        let host = args.get("host").and_then(|v| v.as_str()).unwrap_or("");
        match verdict {
            rules::Verdict::Allow => {
                // 放行不额外写审计：命令执行路径自身已写审计（如 exec → AuditAction::Exec），
                // 此处再写会让每条命令产生双倍记录。裁决详情仅用于 deny/ask。
            }
            rules::Verdict::Deny { source, reason } => {
                audit_flagged(
                    ctx,
                    clum_core::types::AuditAction::CommandDenied,
                    host,
                    "",
                    None,
                    &format!(
                        "deny rule={source} cmd={command} why={reason} segments={:?} hazards={:?}",
                        decision.commands, decision.hazards
                    ),
                    false,
                    None,
                    false,
                    0,
                    Some(&reason),
                )
                .await;
                return Ok(rules::deny_payload(&source, &reason, &command));
            }
            rules::Verdict::Ask { source, reason } => {
                let agent_name = ctx
                    .agent_name
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let caller_group = ctx
                    .caller_group
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let ttl = DEFAULT_APPROVAL_TTL_SECS;
                let approval_id = ctx
                    .approvals
                    .create(crate::approvals::NewApproval {
                        agent_name,
                        caller_group,
                        tool_name: tool_name.to_string(),
                        host_name: host.to_string(),
                        command: command.clone(),
                        rule_id: source.clone(),
                        reason: reason.clone(),
                        ttl_secs: ttl,
                    })
                    .await?;
                let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(ttl)).to_rfc3339();
                audit_flagged(
                    ctx,
                    clum_core::types::AuditAction::ApprovalRequested,
                    host,
                    "",
                    None,
                    &format!(
                        "ask approval={approval_id} rule={source} cmd={command} segments={:?} hazards={:?}",
                        decision.commands, decision.hazards
                    ),
                    false,
                    None,
                    true,
                    0,
                    None,
                )
                .await;
                return Ok(rules::ask_payload(
                    &approval_id,
                    &source,
                    &reason,
                    &command,
                    &expires_at,
                ));
            }
        }
    }

    debug_assert!(
        GROUP_SCOPED_TOOLS.iter().all(|t| matches!(
            *t,
            "audit_query" | "list_recordings" | "get_recording" | "search_recordings"
        )),
        "GROUP_SCOPED_TOOLS 含未知工具名，请勿随意增删"
    );

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
                    Ok(json!({ "ok": false, "error": err_msg }))
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
            let result = get_recording_checked(ctx, path, caller_group.as_deref()).await;
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
                    Ok(json!({ "ok": false, "error": err_msg }))
                }
            }
        }
        "search_recordings" => {
            let caller_group = ctx
                .caller_group
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            match caller_group {
                None => search::search_recordings(ctx, args, None).await,
                Some(cg) => {
                    let allowed: Vec<String> = hosts_in_group(ctx, &cg).await.into_iter().collect();
                    if allowed.is_empty() {
                        // No hosts in this group — return nothing; an empty
                        // filter would otherwise match every recording
                        // (same semantics as audit_query above).
                        Ok(json!({
                            "ok": true,
                            "total": 0,
                            "matches": [],
                            "scanned_files": 0,
                            "scanned_bytes": 0,
                            "decrypt_errors": [],
                        }))
                    } else {
                        search::search_recordings(ctx, args, Some(allowed.as_slice())).await
                    }
                }
            }
        }
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

/// Validate and read a recording file for `get_recording`. Every failure is
/// returned as `Err` so the caller's single audit branch records it.
async fn get_recording_checked(
    ctx: &ToolContext,
    path: &str,
    caller_group: Option<&str>,
) -> Result<Value> {
    if path.is_empty() {
        anyhow::bail!("missing 'path'");
    }
    if !std::path::Path::new(path).is_absolute() {
        anyhow::bail!("path must be absolute (use the path returned by list_recordings)");
    }

    // Canonicalize FIRST, then run the group check against the same
    // resolved path that will be read — otherwise a `..` segment can
    // make the group check see host A while the read targets host B.
    let canonical_root = ctx
        .recordings_dir
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("recordings directory not found"))?;
    let canonical_path = std::path::Path::new(path)
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("failed to resolve path: {e}"))?;
    if !canonical_path.starts_with(&canonical_root) {
        anyhow::bail!("path outside recordings directory");
    }

    if let Some(cg) = caller_group {
        let host_from_path = canonical_path
            .strip_prefix(&canonical_root)
            .ok()
            .and_then(|rel| rel.components().next())
            .and_then(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .unwrap_or_default();
        let host_group = resolve_host_group(ctx, &host_from_path).await;
        if host_group.as_deref() != Some(cg) {
            anyhow::bail!("host '{host_from_path}' is not in your group '{cg}'");
        }
    }

    read_recording_file_at(&canonical_path, &ctx.recording_keyring).await
}

/// Read an already-canonicalized recording path and decrypt its content.
/// The caller is responsible for canonicalizing and confining the path to
/// `recordings_dir` (see `get_recording_checked`), so this helper does no
/// path validation of its own.
async fn read_recording_file_at(
    canonical_path: &std::path::Path,
    keyring: &crate::recording_keyring::RecordingKeyring,
) -> Result<Value> {
    let raw = tokio::fs::read(canonical_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read recording: {e}"))?;
    let plain = keyring.decrypt_or_passthrough(&raw)?;
    let content = String::from_utf8_lossy(&plain).to_string();
    Ok(json!({
        "path": canonical_path.to_string_lossy(),
        "content": content,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HOSTS_YAML: &str =
        "hosts:\n  - name: devhost\n    group: dev\n  - name: prodhost\n    group: prod\n";

    fn grouped_ctx(caller_group: &str) -> (ToolContext, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hosts_path = tmp.path().join("hosts.yaml");
        std::fs::write(&hosts_path, HOSTS_YAML).expect("write hosts.yaml");
        let router = crate::router::HostRouter::from_file(&hosts_path).expect("load router");
        let audit_db =
            crate::audit::AuditDb::open(&tmp.path().join("audit.db")).expect("open audit db");
        let bridge_store = crate::bridge_store::BridgeStore::open(&tmp.path().join("bridge.db"))
            .expect("open bridge store");
        let ctx = ToolContext {
            router: Arc::new(router),
            ca_cert_path: None,
            audit_db: Arc::new(audit_db),
            agent_name: Arc::new(std::sync::Mutex::new("test".to_string())),
            caller_group: Arc::new(std::sync::Mutex::new(Some(caller_group.to_string()))),
            current_op: Arc::new(std::sync::Mutex::new(None)),
            forward_manager: Arc::new(crate::forward::ForwardManager::new()),
            stream_manager: Arc::new(crate::stream::StreamManager::new()),
            recordings_dir: tmp.path().join("recordings"),
            recording_keyring: Arc::new(
                crate::recording_keyring::RecordingKeyring::load_or_create(
                    &tmp.path().join("rec-keys"),
                )
                .unwrap(),
            ),
            bridge_registry: Arc::new(crate::registry::BridgeRegistry::new()),
            bridge_store: Arc::new(bridge_store),
            file_transfer: crate::server_config::FileTransferConfig::default(),
            command_kb: Arc::new(
                clum_command_kb::KnowledgeBase::builtin().expect("builtin command kb"),
            ),
            rules: Arc::new(std::sync::RwLock::new(clum_command_kb::RuleSet::default())),
            rules_path: tmp.path().join("command-rules.toml"),
            approvals: Arc::new(
                crate::approvals::ApprovalStore::open(&tmp.path().join("approvals.db"))
                    .expect("open approval store"),
            ),
        };
        (ctx, tmp)
    }

    fn write_cast(recordings_dir: &std::path::Path, host: &str) -> std::path::PathBuf {
        let dir = recordings_dir.join(host).join("2026-08-06");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.cast");
        std::fs::write(&path, "{\"version\":2}\n[1.0, \"o\", \"hi\"]\n").unwrap();
        path
    }

    fn progress() -> crate::progress::ProgressReporter {
        crate::progress::ProgressReporter::new_stdout(
            None,
            Arc::new(tokio::sync::Mutex::new(tokio::io::stdout())),
        )
    }

    #[tokio::test]
    async fn get_recording_rejects_cross_group_dotdot_path() {
        let (ctx, _tmp) = grouped_ctx("dev");
        write_cast(&ctx.recordings_dir, "devhost");
        write_cast(&ctx.recordings_dir, "prodhost");

        let evil = ctx
            .recordings_dir
            .join("devhost")
            .join("..")
            .join("prodhost")
            .join("2026-08-06")
            .join("x.cast");

        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "get_recording",
            json!({ "path": evil.to_string_lossy() }),
            &mut progress,
        )
        .await
        .expect("rejection must be a business error envelope, not a hard Err");
        assert_eq!(value["ok"], json!(false));
        let err = value["error"].as_str().expect("error message present");
        assert!(
            err.contains("not in your group"),
            "cross-group traversal must be rejected, got: {err}"
        );
        assert!(
            value.get("content").is_none(),
            "rejected path must not leak content"
        );
    }

    #[tokio::test]
    async fn get_recording_rejects_out_of_dir_path_with_error_envelope() {
        let (ctx, tmp) = grouped_ctx("dev");
        std::fs::create_dir_all(&ctx.recordings_dir).unwrap();
        let outside = tmp.path().join("outside.cast");
        std::fs::write(&outside, "{}").unwrap();

        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "get_recording",
            json!({ "path": outside.to_string_lossy() }),
            &mut progress,
        )
        .await
        .expect("rejection must be a business error envelope, not a hard Err");
        assert_eq!(value["ok"], json!(false));
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|e| e.contains("outside recordings directory")),
            "out-of-dir path must be rejected, got: {}",
            value["error"]
        );
    }

    #[tokio::test]
    async fn get_recording_allows_same_group_path() {
        let (ctx, _tmp) = grouped_ctx("dev");
        let path = write_cast(&ctx.recordings_dir, "devhost");

        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "get_recording",
            json!({ "path": path.to_string_lossy() }),
            &mut progress,
        )
        .await
        .expect("same-group recording should succeed");
        assert!(value.get("content").and_then(|v| v.as_str()).is_some());
    }

    #[test]
    fn extracts_command_per_tool() {
        assert_eq!(
            rules::extract_command("exec", &json!({"command": "ls -la"})).as_deref(),
            Some("ls -la")
        );
        assert_eq!(
            rules::extract_command("batch_exec", &json!({"command": "df -h"})).as_deref(),
            Some("df -h")
        );
        assert_eq!(
            rules::extract_command("send_keys", &json!({"keys": "rm -rf /\n"})).as_deref(),
            Some("rm -rf /\n")
        );
        assert_eq!(rules::extract_command("capture_pane", &json!({})), None);
    }

    #[tokio::test]
    async fn denies_destructive_command_via_execute_tool() {
        let (ctx, _tmp) = grouped_ctx("dev");
        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "exec",
            json!({ "host": "devhost", "command": "rm -rf /" }),
            &mut progress,
        )
        .await
        .expect("deny must be a business error envelope, not a hard Err");
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["error_code"], json!("COMMAND_DENIED"));
        assert_eq!(value["rule_id"], json!("kb:rm dangerous-flag(-rf)"));
    }

    #[tokio::test]
    async fn asks_unknown_command_and_records_pending_approval() {
        let (ctx, _tmp) = grouped_ctx("dev");
        let mut progress = progress();
        let value = execute_tool(
            &ctx,
            "exec",
            json!({ "host": "devhost", "command": "systemctl restart nginx" }),
            &mut progress,
        )
        .await
        .expect("ask must be a business error envelope, not a hard Err");
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["error_code"], json!("APPROVAL_REQUIRED"));
        assert!(
            value["approval_id"].as_str().is_some(),
            "ask payload must carry an approval_id"
        );
        assert_eq!(ctx.approvals.list_pending().await.unwrap().len(), 1);
    }
}
