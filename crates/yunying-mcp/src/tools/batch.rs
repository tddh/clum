use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::common::{collect_batch_results, make_semaphore, resolve_hosts};
use super::exec::exec_in_session;
use super::ToolContext;
use crate::files::OverwriteMode;
use crate::transport::connect_via_registry;
use yunying_core::types::AuditAction;

pub(crate) async fn batch_exec(ctx: &ToolContext, args: Value) -> Result<Value> {
    let hosts_arg: Vec<String> = args["hosts"]
        .as_array()
        .context("missing 'hosts'")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if hosts_arg.is_empty() {
        return Ok(
            json!({ "ok": true, "command": "", "total": 0, "success": 0, "failed": 0,
            "total_duration_ms": 0, "results": {}, "error": "empty hosts list" }),
        );
    }

    let command = args["command"].as_str().context("missing 'command'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(600000);
    let max_lines = args["max_lines"]
        .as_u64()
        .map(|v| v as usize)
        .unwrap_or(200);
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(5) as usize;

    let targets = resolve_hosts(ctx, &hosts_arg);
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let registry = std::sync::Arc::clone(&ctx.bridge_registry);
    let cmd = command.to_string();
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();

    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let registry = registry.clone();
        let cmd = cmd.clone();
        let sem = semaphore.clone();

        let handle = tokio::spawn(async move {
            let _permit = if let Some(s) = &sem {
                s.acquire().await.ok()
            } else {
                None
            };

            let host = match host_opt {
                Some(h) => h,
                None => {
                    return (
                        host_name,
                        json!({
                            "ok": false, "output": "", "exit_code": null,
                            "duration_ms": 0, "error": "host not found in registry",
                        }),
                    )
                }
            };

            let mut stream = match connect_via_registry(&registry, &host, &ca_cert).await {
                Ok(s) => s,
                Err(e) => {
                    return (
                        host_name,
                        json!({
                            "ok": false, "output": "", "exit_code": null,
                            "duration_ms": 0, "error": format!("connect: {e}"),
                        }),
                    )
                }
            };

            let session_name = "yunying";

            // 创建 session 并获取 pane_id
            let pane_id = match super::common::create_session_inner(&mut stream, session_name).await
            {
                Ok(resp) => resp
                    .get("pane_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("%0")
                    .to_string(),
                Err(e) => {
                    return (
                        host_name,
                        json!({
                            "ok": false, "output": "", "exit_code": null,
                            "duration_ms": 0, "error": format!("session_create: {e}"),
                        }),
                    )
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
        return Ok(json!({"ok": true, "total": 0, "success": 0, "failed": 0,
            "total_duration_ms": 0, "results": {}, "error": "empty hosts list"}));
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

    let targets = resolve_hosts(ctx, &hosts_arg);
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();
    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let local = local_path.to_string();
        let remote = remote_path.to_string();
        let exclude = exclude.clone();
        let sem = semaphore.clone();
        let mut task_progress = progress.clone();

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
                &ca_cert,
                overwrite,
                &exclude,
                &mut task_progress,
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
        return Ok(json!({"ok": true, "total": 0, "success": 0, "failed": 0,
            "total_duration_ms": 0, "results": {}, "error": "empty hosts list"}));
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

    let targets = resolve_hosts(ctx, &hosts_arg);
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();
    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let remote = remote_path.to_string();
        let local_dir = local_dir.to_string();
        let file_name = file_name.clone();
        let sem = semaphore.clone();
        let mut task_progress = progress.clone();

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
                &ca_cert,
                &mut task_progress,
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
