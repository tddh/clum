use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::common::{collect_batch_results, create_session_inner, make_semaphore, resolve_hosts};
use super::exec::exec_in_session;
use super::ToolContext;
use crate::transport::{connect_to_bridge_hybrid, send_json_frame};
use agent_ops_core::types::{AuditAction, AuditEvent};
use chrono::Utc;
use uuid::Uuid;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn audit(
    ctx: &ToolContext,
    action: AuditAction,
    host: &str,
    session: &str,
    pane_id: Option<&str>,
    detail: &str,
    output_summary: Option<&str>,
    success: bool,
    duration_ms: u64,
    error_message: Option<&str>,
) {
    let agent_name = ctx
        .agent_name
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let event = AuditEvent {
        event_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        agent_name,
        host_name: host.to_string(),
        session_name: session.to_string(),
        pane_id: pane_id.map(|s| s.to_string()),
        action,
        detail: detail.to_string(),
        output_summary: output_summary.map(|s| s.to_string()),
        success,
        duration_ms,
        error_message: error_message.map(|s| s.to_string()),
    };
    ctx.audit_db.log(event).await;
}

pub(crate) async fn deploy_bridge(ctx: &ToolContext, args: Value) -> Result<Value> {
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

    let binary_path = args["binary_path"]
        .as_str()
        .context("missing 'binary_path'")?;
    let user_remote = args["remote_path"].as_str();
    let concurrency_limit = args["concurrency"].as_u64().unwrap_or(3) as usize;

    let metadata = tokio::fs::metadata(binary_path)
        .await
        .with_context(|| format!("binary not found at {}", binary_path))?;
    let binary_size = metadata.len();

    let targets = resolve_hosts(ctx, &hosts_arg);
    let semaphore = make_semaphore(concurrency_limit);
    let ca_cert = ctx.ca_cert_path.clone();
    let start = std::time::Instant::now();

    let mut handles: Vec<tokio::task::JoinHandle<(String, Value)>> = Vec::new();

    for (host_name, host_opt) in targets {
        let ca_cert = ca_cert.clone();
        let binary_path = binary_path.to_string();
        let user_remote = user_remote.map(|s| s.to_string());
        let sem = semaphore.clone();

        handles.push(tokio::spawn(async move {
            let _permit = if let Some(s) = &sem { s.acquire().await.ok() } else { None };

            let host = match host_opt {
                Some(h) => h,
                None => return (host_name.clone(), json!({
                    "ok": false, "status": "host_not_found",
                    "error": "host not found in registry"
                })),
            };

            let mut stream = match connect_to_bridge_hybrid(
                &host.bridge_addr, &host.bridge_token,
                &ca_cert, 3,
            ).await {
                Ok(s) => s,
                Err(e) => return (host_name.clone(), json!({
                    "ok": false, "status": "bridge_unreachable",
                    "error": format!("{:#}", e)
                })),
            };

            let session_name = "agent-ops";
            let pane_id = match create_session_inner(&mut stream, session_name).await {
                Ok(resp) => resp.get("pane_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("%0")
                    .to_string(),
                Err(e) => return (host_name.clone(), json!({
                    "ok": false, "status": "session_failed",
                    "error": format!("{:#}", e)
                })),
            };

            let exec_result = exec_in_session(&mut stream, session_name, &pane_id,
                "systemctl show rmux-bridge -p ExecStart 2>/dev/null | grep -oP 'path=\\K[^ ;]+' || echo ''",
                10000, 50).await;

            // exec 输出为完整终端上下文（含提示符与命令回显），按路径特征提取行
            let systemd_path = exec_result
                .output
                .lines()
                .map(|l| l.trim())
                .find(|l| l.starts_with('/'))
                .unwrap_or("")
                .to_string();
            if systemd_path.is_empty() {
                return (host_name.clone(), json!({
                    "ok": false, "status": "first_time_deploy",
                    "error": "rmux-bridge service not found — use deploy/install-bridge.sh via SSH"
                }));
            }

            let remote_path = match &user_remote {
                Some(u) => {
                    if u != &systemd_path {
                        return (host_name.clone(), json!({
                            "ok": false, "status": "path_mismatch",
                            "error": format!("specified path '{}' does not match systemd ExecStart '{}'", u, systemd_path)
                        }));
                    }
                    u.clone()
                }
                None => systemd_path,
            };

            let upload_new_path = format!("{}.new", remote_path);
            let upload_result = crate::files::upload_file(
                &host, &binary_path, &upload_new_path,
                &ca_cert,
                crate::files::OverwriteMode::Overwrite, &[],
            ).await;

            match upload_result {
                Err(e) => return (host_name.clone(), json!({
                    "ok": false, "status": "upload_failed",
                    "error": format!("{:#}", e)
                })),
                Ok(files) => {
                    let file_failed = files.iter().filter(|f| f.status == "failed").count();
                    if file_failed > 0 {
                        return (host_name.clone(), json!({
                            "ok": false, "status": "upload_failed",
                            "error": format!("{} file(s) failed to upload", file_failed)
                        }));
                    }
                }
            }

            let replace_cmd = format!("chmod +x {new} && mv {new} {path}",
                new = upload_new_path, path = remote_path);
            let replace_result = exec_in_session(&mut stream, session_name, &pane_id,
                &replace_cmd, 10000, 50).await;
            if !replace_result.ok || replace_result.error.is_some() {
                return (host_name.clone(), json!({
                    "ok": false, "status": "replace_failed",
                    "output": replace_result.output,
                    "exit_code": replace_result.exit_code,
                    "error": replace_result.error,
                }));
            }

            // Fire-and-forget: bridge dies immediately, don't wait for sentinel
            let _ = send_json_frame(&mut stream, &json!({
                "type": "send_keys",
                "session_name": session_name,
                "pane_id": pane_id,
                "keys": "\x15c\nsystemctl restart rmux-bridge\n",
            })).await;

            drop(stream);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let mut new_stream = match connect_to_bridge_hybrid(
                &host.bridge_addr, &host.bridge_token,
                &ca_cert, 5,
            ).await {
                Ok(s) => s,
                Err(e) => return (host_name.clone(), json!({
                    "ok": false, "status": "reconnect_failed",
                    "error": format!("bridge did not come back: {:#}", e)
                })),
            };

            let verify_result = exec_in_session(&mut new_stream, session_name, &pane_id,
                "systemctl is-active rmux-bridge", 10000, 50).await;
            let is_active = verify_result.output.lines().any(|l| l.trim() == "active");

            (host_name.clone(), json!({
                "ok": is_active,
                "status": if is_active { "restarted" } else { "verify_failed" },
                "output": verify_result.output,
                "exit_code": verify_result.exit_code,
                "error": if is_active { None } else { Some(format!("bridge not active: {}", verify_result.output.trim())) },
            }))
        }));
    }

    let (results_map, success_count, failed_count) = collect_batch_results(handles).await;
    let total_duration_ms = start.elapsed().as_millis() as u64;

    audit(
        ctx,
        AuditAction::DeployBridge,
        "",
        "",
        None,
        &format!("hosts:{:?} binary:{}", hosts_arg, binary_path),
        None,
        failed_count == 0,
        total_duration_ms,
        None,
    )
    .await;

    Ok(json!({
        "ok": failed_count == 0,
        "binary": binary_path,
        "binary_size": binary_size,
        "total": hosts_arg.len(),
        "success": success_count,
        "failed": failed_count,
        "total_duration_ms": total_duration_ms,
        "results": results_map,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::exec::ExecResult;
    use serde_json::json;

    fn build_exec_response(result: &ExecResult) -> serde_json::Value {
        let mut response = json!({
            "ok": result.ok,
            "output": result.output,
            "exit_code": result.exit_code,
            "duration_ms": result.duration_ms,
            "error": result.error,
        });
        if let Some(ref state) = result.terminal_state {
            response["terminal_state"] = state.clone();
        }
        if let Some(ref cursor) = result.cursor {
            response["cursor"] = cursor.clone();
        }
        if let Some(ref pre_state) = result.pre_terminal_state {
            response["pre_terminal_state"] = pre_state.clone();
        }
        if result.refused {
            response["refused"] = json!(true);
        }
        response
    }

    #[test]
    fn test_exec_result_refused_editor() {
        let result = ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some("Terminal is in editor (vim/nano). Use send_keys to interact with editor, or exit editor first.".to_string()),
            terminal_state: Some(json!("editor")),
            cursor: None,
            pre_terminal_state: Some(json!("editor")),
            refused: true,
        };
        let response = build_exec_response(&result);
        assert_eq!(response["ok"], false);
        assert_eq!(response["refused"], true);
        assert_eq!(response["pre_terminal_state"], "editor");
        assert_eq!(response["terminal_state"], "editor");
        assert!(response["error"].as_str().unwrap().contains("editor"));
        assert!(response["error"].as_str().unwrap().contains("vim"));
    }

    #[test]
    fn test_exec_result_refused_pager() {
        let result = ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some(
                "Terminal is in pager (less/more). Use send_keys('q') to exit pager first."
                    .to_string(),
            ),
            terminal_state: Some(json!("pager")),
            cursor: None,
            pre_terminal_state: Some(json!("pager")),
            refused: true,
        };
        let response = build_exec_response(&result);
        assert_eq!(response["ok"], false);
        assert_eq!(response["refused"], true);
        assert_eq!(response["pre_terminal_state"], "pager");
        assert!(response["error"].as_str().unwrap().contains("pager"));
        assert!(response["error"].as_str().unwrap().contains("less"));
    }

    #[test]
    fn test_exec_result_refused_password() {
        let result = ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some("Terminal is waiting for password. Use send_keys to provide password or Ctrl-C to cancel.".to_string()),
            terminal_state: Some(json!("password")),
            cursor: None,
            pre_terminal_state: Some(json!("password")),
            refused: true,
        };
        let response = build_exec_response(&result);
        assert_eq!(response["ok"], false);
        assert_eq!(response["refused"], true);
        assert_eq!(response["pre_terminal_state"], "password");
        assert!(response["error"].as_str().unwrap().contains("password"));
        assert!(response["error"].as_str().unwrap().contains("Ctrl-C"));
    }

    #[test]
    fn test_exec_result_refused_confirm() {
        let result = ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some(
                "Terminal is waiting for confirmation. Use send_keys to respond.".to_string(),
            ),
            terminal_state: Some(json!("confirm")),
            cursor: None,
            pre_terminal_state: Some(json!("confirm")),
            refused: true,
        };
        let response = build_exec_response(&result);
        assert_eq!(response["ok"], false);
        assert_eq!(response["refused"], true);
        assert_eq!(response["pre_terminal_state"], "confirm");
        assert!(response["error"].as_str().unwrap().contains("confirmation"));
    }

    #[test]
    fn test_exec_result_refused_running() {
        let result = ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some("A process is still running. Use wait_stable/wait_exit to wait, or send_keys(Ctrl-C) to stop it.".to_string()),
            terminal_state: Some(json!("running")),
            cursor: None,
            pre_terminal_state: Some(json!("running")),
            refused: true,
        };
        let response = build_exec_response(&result);
        assert_eq!(response["ok"], false);
        assert_eq!(response["refused"], true);
        assert_eq!(response["pre_terminal_state"], "running");
        assert!(response["error"]
            .as_str()
            .unwrap()
            .contains("still running"));
    }

    #[test]
    fn test_exec_result_refused_repl() {
        let result = ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some("Terminal is in REPL (python3/mysql). Use send_keys to send REPL commands, or exit REPL first.".to_string()),
            terminal_state: Some(json!("repl")),
            cursor: None,
            pre_terminal_state: Some(json!("repl")),
            refused: true,
        };
        let response = build_exec_response(&result);
        assert_eq!(response["ok"], false);
        assert_eq!(response["refused"], true);
        assert_eq!(response["pre_terminal_state"], "repl");
        assert!(response["error"].as_str().unwrap().contains("REPL"));
    }

    #[test]
    fn test_exec_result_refused_unknown() {
        let result = ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some(
                "Terminal state is unknown. Use capture_pane to inspect terminal content."
                    .to_string(),
            ),
            terminal_state: Some(json!("unknown")),
            cursor: None,
            pre_terminal_state: Some(json!("unknown")),
            refused: true,
        };
        let response = build_exec_response(&result);
        assert_eq!(response["ok"], false);
        assert_eq!(response["refused"], true);
        assert_eq!(response["pre_terminal_state"], "unknown");
        assert!(response["error"].as_str().unwrap().contains("unknown"));
        assert!(response["error"].as_str().unwrap().contains("capture_pane"));
    }

    #[test]
    fn test_exec_result_normal_ready() {
        let result = ExecResult {
            ok: true,
            output: "file1.txt\nfile2.txt".to_string(),
            exit_code: Some(0),
            duration_ms: 120,
            error: None,
            terminal_state: Some(json!("ready")),
            cursor: Some(json!({"row": 5, "col": 14, "visible": true})),
            pre_terminal_state: Some(json!("ready")),
            refused: false,
        };
        let response = build_exec_response(&result);
        assert_eq!(response["ok"], true);
        assert!(response.get("refused").is_none());
        assert_eq!(response["pre_terminal_state"], "ready");
        assert_eq!(response["terminal_state"], "ready");
        assert_eq!(response["exit_code"], 0);
        assert_eq!(response["output"], "file1.txt\nfile2.txt");
        assert_eq!(response["duration_ms"], 120);
        assert!(response["error"].is_null());
        assert!(response.get("cursor").is_some());
    }

    #[test]
    fn test_exec_result_error_non_refused() {
        let result = ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some("send_keys: broken pipe".to_string()),
            terminal_state: None,
            cursor: None,
            pre_terminal_state: Some(json!("ready")),
            refused: false,
        };
        let response = build_exec_response(&result);
        assert_eq!(response["ok"], false);
        assert!(response.get("refused").is_none());
        assert_eq!(response["pre_terminal_state"], "ready");
        assert!(response["terminal_state"].is_null());
        assert!(response["error"].as_str().unwrap().contains("broken pipe"));
        assert!(response["exit_code"].is_null());
    }

    #[test]
    fn test_exec_result_command_failed() {
        let result = ExecResult {
            ok: false,
            output: "ls: cannot access '/nonexistent': No such file or directory".to_string(),
            exit_code: Some(2),
            duration_ms: 85,
            error: None,
            terminal_state: Some(json!("ready")),
            cursor: Some(json!({"row": 1, "col": 0, "visible": true})),
            pre_terminal_state: Some(json!("ready")),
            refused: false,
        };
        let response = build_exec_response(&result);
        assert_eq!(response["ok"], false);
        assert!(response.get("refused").is_none());
        assert_eq!(response["exit_code"], 2);
        assert_eq!(response["pre_terminal_state"], "ready");
    }
}
