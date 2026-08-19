use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::common::{collect_batch_results, make_semaphore, resolve_hosts, resolve_pane_id};
use super::exec::{exec_in_session, unescape_keys};
use super::ToolContext;
use crate::files::OverwriteMode;
use crate::transport::{connect_via_registry, recv_json_frame, send_json_frame};
use clum_core::types::AuditAction;
use clum_core::DEFAULT_EXEC_TIMEOUT_MS;

pub(crate) async fn batch_exec(
    ctx: &ToolContext,
    args: Value,
    progress: &crate::progress::ProgressReporter,
) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array()
        .context("missing 'hosts'")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if hosts_arg.is_empty() {
        return Ok(json!({"ok": false, "error": "empty hosts list"}));
    }

    let command = args["command"].as_str().context("missing 'command'")?;
    let timeout_ms = args["timeout_ms"]
        .as_u64()
        .unwrap_or(DEFAULT_EXEC_TIMEOUT_MS);
    let max_lines = args["max_lines"]
        .as_u64()
        .map(|v| v as usize)
        .unwrap_or(200);
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(5) as usize;

    let targets = resolve_hosts(ctx, &hosts_arg).await;
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let registry = std::sync::Arc::clone(&ctx.bridge_registry);
    let cmd = command.to_string();
    let start = std::time::Instant::now();
    let total_hosts = targets.len();
    let completed = std::sync::Arc::new(AtomicUsize::new(0));

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();

    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let registry = registry.clone();
        let cmd = cmd.clone();
        let sem = semaphore.clone();
        let completed = completed.clone();
        let mut task_progress = progress.clone();

        let handle = tokio::spawn(async move {
            let _permit = if let Some(s) = &sem {
                s.acquire().await.ok()
            } else {
                None
            };

            let host = match host_opt {
                Some(h) => h,
                None => {
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    task_progress
                        .report(done as u64, total_hosts as u64, &host_name)
                        .await;
                    return (
                        host_name,
                        json!({
                            "ok": false, "output": "", "exit_code": null,
                            "duration_ms": 0, "error": "host not found in registry",
                        }),
                    );
                }
            };

            let mut stream = match connect_via_registry(&registry, &host, ca_cert.as_deref()).await
            {
                Ok(s) => s,
                Err(e) => {
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    task_progress
                        .report(done as u64, total_hosts as u64, &host_name)
                        .await;
                    return (
                        host_name,
                        json!({
                            "ok": false, "output": "", "exit_code": null,
                            "duration_ms": 0, "error": format!("connect: {e}"),
                        }),
                    );
                }
            };

            let session_name = "clum";

            // 创建 session 并获取 pane_id
            let pane_id = match super::common::create_session_inner(&mut stream, session_name).await
            {
                Ok(resp) => resp
                    .get("pane_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("%0")
                    .to_string(),
                Err(e) => {
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    task_progress
                        .report(done as u64, total_hosts as u64, &host_name)
                        .await;
                    return (
                        host_name,
                        json!({
                            "ok": false, "output": "", "exit_code": null,
                            "duration_ms": 0, "error": format!("session_create: {e}"),
                        }),
                    );
                }
            };

            let result = exec_in_session(
                &mut stream,
                session_name,
                &pane_id,
                &cmd,
                timeout_ms,
                max_lines,
            )
            .await;

            let mut per_host = json!({
                "ok": result.ok && result.error.is_none(),
                "output": result.output,
                "exit_code": result.exit_code,
                "duration_ms": result.duration_ms,
                "error": result.error,
            });
            if let Some(ref state) = result.terminal_state {
                per_host["terminal_state"] = state.clone();
            }
            if let Some(ref cursor) = result.cursor {
                per_host["cursor"] = cursor.clone();
            }
            if let Some(ref pre_state) = result.pre_terminal_state {
                per_host["pre_terminal_state"] = pre_state.clone();
            }
            if result.refused {
                per_host["refused"] = json!(true);
            }

            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            task_progress
                .report(done as u64, total_hosts as u64, &host_name)
                .await;

            (host_name, per_host)
        });

        handles.push(handle);
    }

    let (results_map, success_count, failed_count) = collect_batch_results(handles).await;
    let total_duration_ms = start.elapsed().as_millis() as u64;

    super::audit(
        ctx,
        AuditAction::BatchExec,
        "",
        "",
        None,
        &format!("hosts:{:?} cmd:{}", hosts_arg, cmd),
        None,
        failed_count == 0,
        total_duration_ms,
        None,
    )
    .await;

    Ok(json!({
        "ok": failed_count == 0,
        "command": command,
        "total": hosts_arg.len(),
        "success": success_count,
        "failed": failed_count,
        "total_duration_ms": total_duration_ms,
        "results": results_map,
    }))
}

pub(crate) async fn batch_upload(
    ctx: &ToolContext,
    args: Value,
    progress: &crate::progress::ProgressReporter,
) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array()
        .context("missing 'hosts'")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if hosts_arg.is_empty() {
        return Ok(json!({"ok": false, "error": "empty hosts list"}));
    }

    let local_path = args["local_path"]
        .as_str()
        .context("missing 'local_path'")?;
    let remote_path = args["remote_path"]
        .as_str()
        .context("missing 'remote_path'")?;
    let overwrite = match args["overwrite"].as_str().unwrap_or("overwrite") {
        "skip" => OverwriteMode::Skip,
        "rename" => OverwriteMode::Rename,
        "error" => OverwriteMode::NoClobber,
        _ => OverwriteMode::Overwrite,
    };
    let exclude: Vec<String> = args["exclude"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(5) as usize;

    let targets = resolve_hosts(ctx, &hosts_arg).await;
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let registry = std::sync::Arc::clone(&ctx.bridge_registry);
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();
    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let local = local_path.to_string();
        let remote = remote_path.to_string();
        let exclude = exclude.clone();
        let sem = semaphore.clone();
        let mut task_progress = progress.clone();
        let registry = registry.clone();

        handles.push(tokio::spawn(async move {
            let _permit = if let Some(s) = &sem {
                s.acquire().await.ok()
            } else {
                None
            };
            let host = match host_opt {
                Some(h) => h,
                None => return (host_name, json!({"ok": false, "error": "host not found"})),
            };
            match crate::files::upload_file(
                &host,
                &local,
                &remote,
                ca_cert.as_deref(),
                overwrite,
                &exclude,
                &mut task_progress,
                &registry,
                None,
            )
            .await
            {
                Ok(files) => {
                    let uploaded = files.iter().filter(|f| f.status == "uploaded").count();
                    let file_failed = files.iter().filter(|f| f.status == "failed").count();
                    (
                        host_name,
                        json!({
                            "ok": file_failed == 0,
                            "files": files, "total": files.len(),
                            "uploaded": uploaded, "skipped": files.len() - uploaded - file_failed,
                            "failed_count": file_failed,
                        }),
                    )
                }
                Err(e) => (host_name, json!({"ok": false, "error": e.to_string()})),
            }
        }));
    }

    let (results_map, success_count, failed_count) = collect_batch_results(handles).await;
    let total_duration_ms = start.elapsed().as_millis() as u64;
    super::audit(
        ctx,
        AuditAction::BatchUpload,
        "",
        "",
        None,
        &format!("hosts:{:?} local:{}", hosts_arg, local_path),
        None,
        failed_count == 0,
        total_duration_ms,
        None,
    )
    .await;

    Ok(json!({
        "ok": failed_count == 0, "total": hosts_arg.len(),
        "success": success_count, "failed": failed_count,
        "total_duration_ms": total_duration_ms, "results": results_map,
    }))
}

pub(crate) async fn batch_download(
    ctx: &ToolContext,
    args: Value,
    progress: &crate::progress::ProgressReporter,
) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array()
        .context("missing 'hosts'")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if hosts_arg.is_empty() {
        return Ok(json!({"ok": false, "error": "empty hosts list"}));
    }

    let remote_path = args["remote_path"]
        .as_str()
        .context("missing 'remote_path'")?;
    let local_dir = args["local_dir"].as_str().context("missing 'local_dir'")?;
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(5) as usize;

    let file_name = std::path::Path::new(remote_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| remote_path.to_string());

    let targets = resolve_hosts(ctx, &hosts_arg).await;
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let registry = std::sync::Arc::clone(&ctx.bridge_registry);
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();
    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let remote = remote_path.to_string();
        let local_dir = local_dir.to_string();
        let file_name = file_name.clone();
        let sem = semaphore.clone();
        let mut task_progress = progress.clone();
        let registry = registry.clone();

        handles.push(tokio::spawn(async move {
            let _permit = if let Some(s) = &sem {
                s.acquire().await.ok()
            } else {
                None
            };
            let host = match host_opt {
                Some(h) => h,
                None => {
                    return (
                        host_name.clone(),
                        json!({"ok": false, "error": "host not found"}),
                    )
                }
            };
            let local_path = format!("{}/{}/{}", local_dir, host_name, file_name);
            if let Some(parent) = std::path::Path::new(&local_path).parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return (
                        host_name.clone(),
                        json!({"ok": false, "error": format!("mkdir: {e}")}),
                    );
                }
            }
            match crate::files::download_file(
                &host,
                &remote,
                &local_path,
                ca_cert.as_deref(),
                &mut task_progress,
                &registry,
                None,
            )
            .await
            {
                Ok(files) => {
                    if files.len() == 1 {
                        (
                            host_name,
                            json!({
                                "ok": true,
                                "file": {"remote_path": remote, "local_path": files[0].path,
                                          "size": files[0].size, "sha256": files[0].sha256}
                            }),
                        )
                    } else {
                        (
                            host_name,
                            json!({
                                "ok": true,
                                "files": files,
                                "total": files.len(),
                            }),
                        )
                    }
                }
                Err(e) => (host_name, json!({"ok": false, "error": e.to_string()})),
            }
        }));
    }

    let (results_map, success_count, failed_count) = collect_batch_results(handles).await;
    let total_duration_ms = start.elapsed().as_millis() as u64;
    super::audit(
        ctx,
        AuditAction::BatchDownload,
        "",
        "",
        None,
        &format!("hosts:{:?} remote:{}", hosts_arg, remote_path),
        None,
        failed_count == 0,
        total_duration_ms,
        None,
    )
    .await;

    Ok(json!({
        "ok": failed_count == 0, "total": hosts_arg.len(),
        "success": success_count, "failed": failed_count,
        "total_duration_ms": total_duration_ms, "results": results_map,
    }))
}

pub(crate) async fn batch_send_keys(
    ctx: &ToolContext,
    args: Value,
    progress: &crate::progress::ProgressReporter,
) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array()
        .context("missing 'hosts'")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if hosts_arg.is_empty() {
        return Ok(json!({"ok": false, "error": "empty hosts list"}));
    }

    let session_name = args["session_name"].as_str().unwrap_or("clum").to_string();
    let pane_id_arg = args["pane_id"].as_str().map(String::from);
    let keys = unescape_keys(args["keys"].as_str().context("missing 'keys'")?);
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(5) as usize;

    let targets = resolve_hosts(ctx, &hosts_arg).await;
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let registry = std::sync::Arc::clone(&ctx.bridge_registry);
    let start = std::time::Instant::now();
    let total_hosts = targets.len();
    let completed = std::sync::Arc::new(AtomicUsize::new(0));

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();

    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let registry = registry.clone();
        let session_name = session_name.clone();
        let pane_id_arg = pane_id_arg.clone();
        let keys = keys.clone();
        let sem = semaphore.clone();
        let completed = completed.clone();
        let mut task_progress = progress.clone();

        let handle = tokio::spawn(async move {
            let _permit = if let Some(s) = &sem {
                s.acquire().await.ok()
            } else {
                None
            };

            let host = match host_opt {
                Some(h) => h,
                None => {
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    task_progress
                        .report(done as u64, total_hosts as u64, &host_name)
                        .await;
                    return (
                        host_name,
                        json!({"ok": false, "error": "host not found in registry"}),
                    );
                }
            };

            let mut stream = match connect_via_registry(&registry, &host, ca_cert.as_deref()).await
            {
                Ok(s) => s,
                Err(e) => {
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    task_progress
                        .report(done as u64, total_hosts as u64, &host_name)
                        .await;
                    return (
                        host_name,
                        json!({"ok": false, "error": format!("connect: {e}")}),
                    );
                }
            };

            let (pane_id, _auto_resolved) =
                match resolve_pane_id(&mut stream, &session_name, pane_id_arg.as_deref()).await {
                    Ok(v) => v,
                    Err(e) => {
                        let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                        task_progress
                            .report(done as u64, total_hosts as u64, &host_name)
                            .await;
                        return (
                            host_name,
                            json!({"ok": false, "error": format!("resolve_pane: {e}")}),
                        );
                    }
                };

            // 投递确认语义（与 send_keys 一致）：发完即返回，不等待命令执行结果。
            if let Err(e) = send_json_frame(
                &mut stream,
                &json!({"type": "send_keys", "session_name": session_name, "pane_id": pane_id.clone(), "keys": keys}),
            )
            .await
            {
                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                task_progress
                    .report(done as u64, total_hosts as u64, &host_name)
                    .await;
                return (
                    host_name,
                    json!({"ok": false, "error": format!("send_keys: {e}")}),
                );
            }
            let result = match recv_json_frame(&mut stream).await {
                Ok(resp) => resp,
                Err(e) => {
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    task_progress
                        .report(done as u64, total_hosts as u64, &host_name)
                        .await;
                    return (
                        host_name,
                        json!({"ok": false, "error": format!("send_keys: {e}")}),
                    );
                }
            };

            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            task_progress
                .report(done as u64, total_hosts as u64, &host_name)
                .await;

            if result["ok"].as_bool().unwrap_or(false) {
                (host_name, json!({"ok": true, "pane_id": pane_id}))
            } else {
                (
                    host_name,
                    json!({"ok": false, "error": result["error"].clone()}),
                )
            }
        });

        handles.push(handle);
    }

    let (results_map, success_count, failed_count) = collect_batch_results(handles).await;
    let total_duration_ms = start.elapsed().as_millis() as u64;

    super::audit(
        ctx,
        AuditAction::BatchSendKeys,
        "",
        "",
        None,
        &format!("hosts:{:?} keys:{}", hosts_arg, keys),
        None,
        failed_count == 0,
        total_duration_ms,
        None,
    )
    .await;

    Ok(json!({
        "ok": failed_count == 0,
        "total": hosts_arg.len(),
        "success": success_count,
        "failed": failed_count,
        "total_duration_ms": total_duration_ms,
        "results": results_map,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    /// 构造一个最小可用的 ToolContext（空 hosts 注册表、临时 db/目录）。
    /// 返回 TempDir 保持临时文件在测试期间存活。
    fn test_ctx() -> (ToolContext, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hosts_path = tmp.path().join("hosts.yaml");
        std::fs::write(&hosts_path, "hosts: []\n").expect("write hosts.yaml");
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
            caller_group: Arc::new(std::sync::Mutex::new(None)),
            current_op: Arc::new(std::sync::Mutex::new(None)),
            forward_manager: Arc::new(crate::forward::ForwardManager::new()),
            stream_manager: Arc::new(crate::stream::StreamManager::new()),
            recordings_dir: tmp.path().to_path_buf(),
            bridge_registry: Arc::new(crate::registry::BridgeRegistry::new()),
            bridge_store: Arc::new(bridge_store),
            file_transfer: crate::server_config::FileTransferConfig::default(),
        };
        (ctx, tmp)
    }

    fn test_progress() -> crate::progress::ProgressReporter {
        crate::progress::ProgressReporter::new_stdout(
            None,
            Arc::new(tokio::sync::Mutex::new(tokio::io::stdout())),
        )
    }

    // ── 空 hosts 校验 ────────────────────────────────────────────

    #[tokio::test]
    async fn batch_exec_rejects_empty_hosts() {
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let result = batch_exec(&ctx, json!({"hosts": []}), &progress)
            .await
            .expect("no error");
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["error"], json!("empty hosts list"));
    }

    #[tokio::test]
    async fn batch_upload_rejects_empty_hosts() {
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let result = batch_upload(&ctx, json!({"hosts": []}), &progress)
            .await
            .expect("no error");
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["error"], json!("empty hosts list"));
    }

    #[tokio::test]
    async fn batch_download_rejects_empty_hosts() {
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let result = batch_download(&ctx, json!({"hosts": []}), &progress)
            .await
            .expect("no error");
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["error"], json!("empty hosts list"));
    }

    #[tokio::test]
    async fn batch_send_keys_rejects_empty_hosts() {
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let result = batch_send_keys(&ctx, json!({"hosts": []}), &progress)
            .await
            .expect("no error");
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["error"], json!("empty hosts list"));
    }

    // ── 缺少必填参数 ────────────────────────────────────────────

    #[tokio::test]
    async fn batch_exec_missing_hosts_is_error() {
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let err = batch_exec(&ctx, json!({}), &progress).await.unwrap_err();
        assert!(
            err.to_string().contains("hosts"),
            "应报 missing hosts: {err}"
        );
    }

    #[tokio::test]
    async fn batch_exec_missing_command_is_error() {
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let err = batch_exec(&ctx, json!({"hosts": ["h1"]}), &progress)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("command"),
            "应报 missing command: {err}"
        );
    }

    #[tokio::test]
    async fn batch_upload_missing_paths_is_error() {
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let err = batch_upload(&ctx, json!({"hosts": ["h1"]}), &progress)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("local_path"),
            "应报 missing local_path: {err}"
        );
    }

    #[tokio::test]
    async fn batch_download_missing_paths_is_error() {
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let err = batch_download(&ctx, json!({"hosts": ["h1"]}), &progress)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("remote_path"),
            "应报 missing remote_path: {err}"
        );
    }

    #[tokio::test]
    async fn batch_send_keys_missing_keys_is_error() {
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let err = batch_send_keys(&ctx, json!({"hosts": ["h1"]}), &progress)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("keys"), "应报 missing keys: {err}");
    }

    // ── 未知主机（无网络路径） ───────────────────────────────────

    #[tokio::test]
    async fn batch_exec_unknown_host_reports_per_host_failure() {
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let result = batch_exec(
            &ctx,
            json!({"hosts": ["no-such-host"], "command": "ls"}),
            &progress,
        )
        .await
        .expect("no error");
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["total"], json!(1));
        assert_eq!(result["success"], json!(0));
        assert_eq!(result["failed"], json!(1));
        let per_host = &result["results"]["no-such-host"];
        assert_eq!(per_host["ok"], json!(false));
        assert!(
            per_host["error"]
                .as_str()
                .unwrap_or("")
                .contains("host not found"),
            "应报 host not found: {per_host}"
        );
    }

    #[tokio::test]
    async fn batch_exec_unknown_hosts_aggregates_mixed_results() {
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let result = batch_exec(
            &ctx,
            json!({"hosts": ["a", "b", "c"], "command": "ls"}),
            &progress,
        )
        .await
        .expect("no error");
        assert_eq!(result["total"], json!(3));
        assert_eq!(result["success"], json!(0));
        assert_eq!(result["failed"], json!(3));
        assert_eq!(result["results"].as_object().unwrap().len(), 3);
    }

    // ── 参数解析 ────────────────────────────────────────────────

    #[tokio::test]
    async fn batch_exec_accepts_optional_params_on_unknown_host() {
        // 未知主机路径不触发网络；timeout_ms/max_lines 被解析后随任务丢弃
        let (ctx, _tmp) = test_ctx();
        let progress = test_progress();
        let result = batch_exec(
            &ctx,
            json!({"hosts": ["x"], "command": "ls", "timeout_ms": 9999, "max_lines": 5}),
            &progress,
        )
        .await
        .expect("no error");
        assert_eq!(result["failed"], json!(1));
    }
}
