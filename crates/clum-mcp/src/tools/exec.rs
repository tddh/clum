use anyhow::{Context, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::ToolContext;
use crate::transport::{connect_to_host, recv_json_frame, send_json_frame};
use clum_core::backoff::FullJitterBackoff;
use clum_core::types::AuditAction;
use clum_core::{DEFAULT_COLLECT_TIMEOUT_MS, DEFAULT_EXEC_TIMEOUT_MS};

/// 将字面量转义序列转为实际控制字符。
/// 兜底处理 OpenCode 等 MCP 客户端未正确 JSON-转义的情况。
pub(crate) fn unescape_keys(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('x') => {
                let hi = chars.next();
                let lo = chars.next();
                match (
                    hi.and_then(|c| c.to_digit(16)),
                    lo.and_then(|c| c.to_digit(16)),
                ) {
                    (Some(h), Some(l)) => out.push(((h << 4) | l) as u8 as char),
                    _ => {
                        // 非完整两位 hex（\x4、\x、\xzz）：原样保留，不注入 NUL
                        out.push('\\');
                        out.push('x');
                        if let Some(h) = hi {
                            out.push(h);
                        }
                        if let Some(l) = lo {
                            out.push(l);
                        }
                    }
                }
            }
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

pub(crate) async fn shell_command(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().unwrap_or("clum");
    let pane_id_arg = args["pane_id"].as_str();
    let command = args["command"].as_str().context("missing 'command'")?;
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let mut tls = connect_to_host(ctx, &host).await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;
    send_json_frame(&mut tls, &json!({ "type": "shell_command", "session_name": session_name, "pane_id": pane_id, "command": command })).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::ShellCommand,
        host_name,
        session_name,
        Some(&pane_id),
        command,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn broadcast_keys(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().unwrap_or("clum");
    let pane_ids = args["pane_ids"].as_array().cloned().unwrap_or_default();
    let keys = args["keys"].as_str().context("missing 'keys'")?;
    let keys = unescape_keys(keys);
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let mut tls = connect_to_host(ctx, &host).await?;
    send_json_frame(&mut tls, &json!({ "type": "broadcast_keys", "session_name": session_name, "pane_ids": pane_ids, "keys": keys })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::BroadcastKeys,
        host_name,
        session_name,
        None,
        &format!("{} panes: {}", pane_ids.len(), keys),
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn cmd_escape(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let cmd_args = args["args"].as_array().cloned().unwrap_or_default();
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let mut tls = connect_to_host(ctx, &host).await?;
    send_json_frame(
        &mut tls,
        &json!({ "type": "cmd_escape", "host": host_name, "args": cmd_args }),
    )
    .await?;
    let response = recv_json_frame(&mut tls).await?;
    let stdout = response["stdout"]
        .as_str()
        .or_else(|| response["output"].as_str())
        .unwrap_or("");
    let stdout_sliced: String = stdout.chars().take(500).collect();
    let output_summary = if stdout_sliced.is_empty() {
        None
    } else {
        Some(stdout_sliced.as_str())
    };
    super::audit(
        ctx,
        AuditAction::CmdEscape,
        host_name,
        "",
        None,
        &format!("{:?}", cmd_args),
        output_summary,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

/// 单次命令执行的结果，用于 batch 聚合和单机 exec 复用
pub(crate) struct ExecResult {
    pub(crate) ok: bool,
    pub(crate) output: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) duration_ms: u64,
    pub(crate) error: Option<String>,
    pub(crate) terminal_state: Option<serde_json::Value>,
    pub(crate) cursor: Option<serde_json::Value>,
    pub(crate) pre_terminal_state: Option<serde_json::Value>,
    pub(crate) refused: bool,
}

/// probe 初始窗口行数：大多数命令输出在此范围内，一次 capture 完成
const PROBE_WINDOW: i64 = 20;
/// 窗口扩大上限：与 daemon 默认 history-limit（50000）对齐
const SCROLLBACK_CAP: i64 = 50000;

/// 命令发出后的状态：marker 与计时信息，用于断连重连后继续等待 sentinel。
struct SentCommand {
    precheck_state: Option<serde_json::Value>,
    start_marker: String,
    sentinel_marker: String,
    timeout_ms: u64,
    started_at: std::time::Instant,
}

enum SendOutcome {
    Sent(SentCommand),
    Done(ExecResult),
}

/// 单轮等待的结果：Done 为终态（含成功/超时/业务错误），Lost 表示连接丢失。
/// sentinel 是远端 pane 上的持久状态，Lost 后换连接继续等即可恢复。
enum AwaitOutcome {
    Done(ExecResult),
    Lost,
}

/// precheck + send_keys：命令发出前的阶段。此阶段连接失败直接报错，
/// 不做自动重试——无法确定命令是否已发出，自动重发可能导致命令重复执行。
async fn exec_send<S>(
    stream: &mut S,
    session_name: &str,
    pane_id: &str,
    command: &str,
    timeout_ms: u64,
) -> SendOutcome
where
    S: tokio::io::AsyncReadExt + tokio::io::AsyncWriteExt + Unpin,
{
    // ── Pre-execution safety check ──
    // Capture terminal state before sending any keys. If the terminal is not in
    // "ready" state (e.g. editor, pager, password prompt), refuse to execute
    // to prevent command injection into non-shell contexts.
    let precheck_state = {
        let check_req = json!({
            "type": "capture_pane",
            "session_name": session_name,
            "pane_id": pane_id,
            "max_lines": 1,
        });
        match send_json_frame(stream, &check_req).await {
            Ok(_) => match recv_json_frame(stream).await {
                Ok(resp) => resp.get("terminal_state").cloned(),
                Err(e) => {
                    tracing::warn!(error = %e, "precheck: failed to receive capture_pane response");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "precheck: failed to send capture_pane request");
                None
            }
        }
    };

    let is_ready = precheck_state
        .as_ref()
        .and_then(|v| v.as_str())
        .map(|s| s == "ready")
        .unwrap_or(true); // If detection fails, allow execution (backward compatible)

    if !is_ready {
        let state_name = precheck_state
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let suggestion = match state_name {
            "editor" => "Terminal is in editor (vim/nano). Use send_keys to interact with editor, or exit editor first.",
            "pager" => "Terminal is in pager (less/more). Use send_keys('q') to exit pager first.",
            "password" => "Terminal is waiting for password. Use send_keys to provide password or Ctrl-C to cancel.",
            "confirm" => "Terminal is waiting for confirmation. Use send_keys to respond.",
            "running" => "A process is still running. Use wait_stable/wait_exit to wait, or send_keys(Ctrl-C) to stop it.",
            "repl" => "Terminal is in REPL (python3/mysql). Use send_keys to send REPL commands, or exit REPL first.",
            _ => "Terminal state is unknown. Use capture_pane to inspect terminal content.",
        };

        return SendOutcome::Done(ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some(suggestion.to_string()),
            terminal_state: precheck_state.clone(),
            cursor: None,
            pre_terminal_state: precheck_state,
            refused: true,
        });
    }

    let marker_id = Uuid::new_v4().to_string();
    let marker_id = &marker_id[..3];
    let start_marker = format!("[{}]", marker_id);
    let sentinel_marker = format!("[{} ", marker_id);

    let keys = format!(
        "\x15echo '{s}'\n{c}\necho \"{e}$?]\"\n",
        s = start_marker,
        c = command,
        e = sentinel_marker
    );

    let send_failed = |e: String| {
        SendOutcome::Done(ExecResult {
            ok: false,
            output: String::new(),
            exit_code: None,
            duration_ms: 0,
            error: Some(e),
            terminal_state: None,
            cursor: None,
            pre_terminal_state: precheck_state.clone(),
            refused: false,
        })
    };

    if let Err(e) = send_json_frame(
        stream,
        &json!({
            "type": "send_keys",
            "session_name": session_name,
            "pane_id": pane_id,
            "keys": keys,
        }),
    )
    .await
    {
        return send_failed(format!("send_keys: {e}"));
    }

    let send_resp = match recv_json_frame(stream).await {
        Ok(r) => r,
        Err(e) => return send_failed(format!("send_keys: {e}")),
    };

    if !send_resp["ok"].as_bool().unwrap_or(false) {
        let err = send_resp["error"]
            .as_str()
            .unwrap_or("send_keys failed")
            .to_string();
        return send_failed(err);
    }

    SendOutcome::Sent(SentCommand {
        precheck_state,
        start_marker,
        sentinel_marker,
        timeout_ms,
        started_at: std::time::Instant::now(),
    })
}

/// 单轮等待 sentinel：先小窗口探测（覆盖快命令与重连后命令已完成的场景），
/// 未出现则 wait_for_text 等待剩余预算；出现后指数扩大窗口直到 start_marker
/// 进入视野，保证大输出完整且小输出不多传。
async fn exec_await_once<S>(
    stream: &mut S,
    session_name: &str,
    pane_id: &str,
    sent: &SentCommand,
    max_lines: usize,
) -> AwaitOutcome
where
    S: tokio::io::AsyncReadExt + tokio::io::AsyncWriteExt + Unpin,
{
    let (probe_text, probe_state, probe_cursor) =
        match capture_window(stream, session_name, pane_id, PROBE_WINDOW).await {
            Ok(r) => r,
            Err(_) => return AwaitOutcome::Lost,
        };

    let mut last_state: Option<serde_json::Value> = probe_state;
    let mut last_cursor: Option<serde_json::Value> = probe_cursor;
    let mut text = probe_text;
    if text.rfind(&sent.sentinel_marker).is_none() {
        let deadline = sent.started_at + std::time::Duration::from_millis(sent.timeout_ms);
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return AwaitOutcome::Done(timeout_exec_result(sent, text, None, None, None));
        }

        // 用 bridge 端 event-driven 的 wait_for_text 等待 sentinel 标记出现
        if send_json_frame(
            stream,
            &json!({
                "type": "wait_for_text",
                "session_name": session_name,
                "pane_id": pane_id,
                "text": sent.sentinel_marker.as_str(),
                "timeout_ms": remaining.as_millis() as u64,
            }),
        )
        .await
        .is_err()
        {
            return AwaitOutcome::Lost;
        }
        let wait_resp = match recv_json_frame(stream).await {
            Ok(r) => r,
            Err(_) => return AwaitOutcome::Lost,
        };

        // bridge 协议约定：超时返回 {"ok":false,"found":false}；
        // 真实错误（pane 不存在、session 丢失等）返回 {"ok":false,"error":...} 且无 found 字段。
        // 后者必须如实上报，不能误报为超时。
        if wait_resp["ok"].as_bool() == Some(false) && wait_resp.get("found").is_none() {
            return AwaitOutcome::Done(ExecResult {
                ok: false,
                output: text,
                exit_code: None,
                duration_ms: sent.started_at.elapsed().as_millis() as u64,
                error: Some(format!(
                    "wait_for_text: {}",
                    wait_resp["error"]
                        .as_str()
                        .unwrap_or("unknown bridge error")
                )),
                terminal_state: None,
                cursor: None,
                pre_terminal_state: sent.precheck_state.clone(),
                refused: false,
            });
        }

        if !wait_resp["found"].as_bool().unwrap_or(false) {
            return AwaitOutcome::Done(timeout_exec_result(sent, text, None, None, None));
        }

        // sentinel 刚出现：重新取小窗口（wait 期间屏幕已变化）
        let (t, s, c) = match capture_window(stream, session_name, pane_id, PROBE_WINDOW).await {
            Ok(r) => r,
            Err(_) => return AwaitOutcome::Lost,
        };
        text = t;
        last_state = s;
        last_cursor = c;
    }

    // sentinel 已在窗口内：指数扩大窗口直到 start_marker 进入视野
    let (mut full_text, window, s, c) =
        match fetch_until_marker(stream, session_name, pane_id, sent, text, PROBE_WINDOW).await {
            Ok(r) => r,
            Err(_) => return AwaitOutcome::Lost,
        };
    if s.is_some() {
        last_state = s;
    }
    if c.is_some() {
        last_cursor = c;
    }

    // wait/probe 的文本匹配可能命中 echo 回显（此时 "$?" 尚未展开为数字，
    // 输出行还没上屏）：等 exit code 可解析（输出行上屏）再解析，
    // 否则会把 "$?" 字面当退出码导致 exit_code=None
    for _ in 0..6 {
        if parse_exit_code(&full_text, &sent.sentinel_marker).is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (t, s, c) = match capture_window(stream, session_name, pane_id, window).await {
            Ok(r) => r,
            Err(_) => return AwaitOutcome::Lost,
        };
        full_text = t;
        if s.is_some() {
            last_state = s;
        }
        if c.is_some() {
            last_cursor = c;
        }
    }

    AwaitOutcome::Done(parse_exec_output(
        full_text,
        last_state,
        last_cursor,
        sent,
        max_lines,
    ))
}

/// capture pane 尾部 window 行（start_line 负值，走 scrollback）。
/// 附带 bridge 返回的 terminal_state / cursor，供 exec 成功路径透传给调用方。
async fn capture_window<S>(
    stream: &mut S,
    session_name: &str,
    pane_id: &str,
    window: i64,
) -> Result<(String, Option<serde_json::Value>, Option<serde_json::Value>), ()>
where
    S: tokio::io::AsyncReadExt + tokio::io::AsyncWriteExt + Unpin,
{
    let req = json!({
        "type": "capture_pane",
        "session_name": session_name,
        "pane_id": pane_id,
        "start_line": -window,
    });
    send_json_frame(stream, &req).await.map_err(|_| ())?;
    let resp = recv_json_frame(stream).await.map_err(|_| ())?;
    Ok((
        resp["text"].as_str().unwrap_or("").to_string(),
        resp.get("terminal_state").cloned(),
        resp.get("cursor").cloned(),
    ))
}

/// 从已有窗口文本开始，指数扩大（20→40→…→SCROLLBACK_CAP）直到 start_marker
/// 进入视野。marker 已滚出 scrollback 时返回最大窗口（parse 走 fallback）。
/// 返回（文本， 最终窗口行数）供后续按同窗口重取。
async fn fetch_until_marker<S>(
    stream: &mut S,
    session_name: &str,
    pane_id: &str,
    sent: &SentCommand,
    mut text: String,
    mut window: i64,
) -> Result<
    (
        String,
        i64,
        Option<serde_json::Value>,
        Option<serde_json::Value>,
    ),
    (),
>
where
    S: tokio::io::AsyncReadExt + tokio::io::AsyncWriteExt + Unpin,
{
    let mut state = None;
    let mut cursor = None;
    loop {
        if text.rfind(&sent.start_marker).is_some() {
            return Ok((text, window, state, cursor));
        }
        if window >= SCROLLBACK_CAP {
            return Ok((text, window, state, cursor));
        }
        window = (window * 2).min(SCROLLBACK_CAP);
        let (t, s, c) = capture_window(stream, session_name, pane_id, window).await?;
        text = t;
        state = s;
        cursor = c;
    }
}

/// 从 sentinel 最后一次出现之后解析退出码（"[xxx N]" 输出行）。
/// 命中 echo 回显（"$?" 尚未展开为数字）时返回 None。
fn parse_exit_code(full_text: &str, sentinel_marker: &str) -> Option<i32> {
    let pos = full_text.rfind(sentinel_marker)?;
    let after = &full_text[pos + sentinel_marker.len()..];
    let digits: String = after
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// sentinel 已出现后，从 capture 文本截取 start_marker → sentinel 区间（含提示符、
/// 命令回显等完整上下文，不做行级过滤），解析退出码并按 max_lines 截断尾部。
fn parse_exec_output(
    full_text: String,
    terminal_state: Option<serde_json::Value>,
    cursor: Option<serde_json::Value>,
    sent: &SentCommand,
    max_lines: usize,
) -> ExecResult {
    let sentinel_marker = sent.sentinel_marker.as_str();
    let duration_ms = sent.started_at.elapsed().as_millis() as u64;

    // rfind：echo sentinel 的命令行也含 sentinel 子串，取最后一次出现
    // （输出行 "[xxx N]"）才能截出完整段落且不留残片
    if let Some(pos) = full_text.rfind(sentinel_marker) {
        let exit_code = parse_exit_code(&full_text, sentinel_marker);

        let output_before_sentinel = &full_text[..pos];
        let current_output =
            if let Some(start_pos) = output_before_sentinel.rfind(&sent.start_marker) {
                let after_start = start_pos + sent.start_marker.len();
                &output_before_sentinel[after_start..]
            } else {
                &full_text[..pos]
            };

        let trimmed = current_output.trim();
        let output = if max_lines > 0 {
            let lines: Vec<&str> = trimmed.lines().collect();
            if lines.len() > max_lines {
                lines[lines.len() - max_lines..].join("\n")
            } else {
                trimmed.to_string()
            }
        } else {
            trimmed.to_string()
        };

        return ExecResult {
            ok: exit_code == Some(0),
            output,
            exit_code,
            duration_ms,
            error: None,
            terminal_state,
            cursor,
            pre_terminal_state: sent.precheck_state.clone(),
            refused: false,
        };
    }

    ExecResult {
        ok: false,
        output: full_text,
        exit_code: None,
        duration_ms,
        error: Some("sentinel not found in captured output".to_string()),
        terminal_state,
        cursor,
        pre_terminal_state: sent.precheck_state.clone(),
        refused: false,
    }
}

fn timeout_exec_result(
    sent: &SentCommand,
    full_text: String,
    terminal_state: Option<serde_json::Value>,
    cursor: Option<serde_json::Value>,
    reason: Option<&str>,
) -> ExecResult {
    let error = match reason {
        Some(r) => format!(
            "timeout waiting for sentinel after {}ms ({})",
            sent.timeout_ms, r
        ),
        None => format!("timeout waiting for sentinel after {}ms", sent.timeout_ms),
    };
    ExecResult {
        ok: false,
        output: full_text,
        exit_code: None,
        duration_ms: sent.started_at.elapsed().as_millis() as u64,
        error: Some(error),
        terminal_state,
        cursor,
        pre_terminal_state: sent.precheck_state.clone(),
        refused: false,
    }
}

/// 在已有 session + pane 中执行一次性命令并等待结果。
/// 供 `batch_exec` 复用：单轮等待，不做断连重连（batch 场景 host 级失败直接上报）。
/// 不负责建连、建 session、写 audit。
pub(crate) async fn exec_in_session<S>(
    stream: &mut S,
    session_name: &str,
    pane_id: &str,
    command: &str,
    timeout_ms: u64,
    max_lines: usize,
) -> ExecResult
where
    S: tokio::io::AsyncReadExt + tokio::io::AsyncWriteExt + Unpin,
{
    match exec_send(stream, session_name, pane_id, command, timeout_ms).await {
        SendOutcome::Done(r) => r,
        SendOutcome::Sent(sent) => {
            match exec_await_once(stream, session_name, pane_id, &sent, max_lines).await {
                AwaitOutcome::Done(r) => r,
                AwaitOutcome::Lost => ExecResult {
                    ok: false,
                    output: String::new(),
                    exit_code: None,
                    duration_ms: sent.started_at.elapsed().as_millis() as u64,
                    error: Some("connection lost while waiting for command".to_string()),
                    terminal_state: None,
                    cursor: None,
                    pre_terminal_state: sent.precheck_state.clone(),
                    refused: false,
                },
            }
        }
    }
}

pub(crate) async fn exec(
    ctx: &ToolContext,
    args: Value,
    progress: &mut crate::progress::ProgressReporter,
) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().unwrap_or("clum");
    let pane_id_arg = args["pane_id"].as_str();
    let command = args["command"].as_str().context("missing 'command'")?;
    let timeout_ms = args["timeout_ms"]
        .as_u64()
        .unwrap_or(DEFAULT_EXEC_TIMEOUT_MS);
    let max_lines = args["max_lines"]
        .as_u64()
        .map(|v| v as usize)
        .unwrap_or(200);
    let clear_screen = args["clear_screen"].as_bool().unwrap_or(false);

    let host = super::common::resolve_host_config(ctx, host_name).await?;
    progress.report(0, 3, "connecting").await;
    let mut tls = connect_to_host(ctx, &host).await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;

    // clear_screen 在 exec_in_session 之前单独处理
    if clear_screen {
        let _ = send_json_frame(
            &mut tls,
            &json!({
                "type": "send_keys", "session_name": session_name, "pane_id": pane_id,
                "keys": "clear\n",
            }),
        )
        .await;
        let _ = recv_json_frame(&mut tls).await;
    }

    progress.report(1, 3, "executing").await;
    let result = match exec_send(&mut tls, session_name, &pane_id, command, timeout_ms).await {
        SendOutcome::Done(r) => r,
        SendOutcome::Sent(sent) => {
            let deadline = sent.started_at + std::time::Duration::from_millis(timeout_ms);
            'outer: loop {
                match exec_await_once(&mut tls, session_name, &pane_id, &sent, max_lines).await {
                    AwaitOutcome::Done(r) => break 'outer r,
                    AwaitOutcome::Lost => {
                        // sentinel 是远端 pane 上的持久状态，断连不影响命令执行；
                        // 退避重连后继续等待，重连耗时计入总预算
                        let mut backoff = FullJitterBackoff::new(
                            std::time::Duration::from_millis(500),
                            std::time::Duration::from_secs(5),
                        );
                        loop {
                            let now = std::time::Instant::now();
                            if now >= deadline {
                                break 'outer timeout_exec_result(
                                    &sent,
                                    String::new(),
                                    None,
                                    None,
                                    Some("connection lost and reconnect failed"),
                                );
                            }
                            tokio::time::sleep(backoff.next_delay().min(deadline - now)).await;
                            if let Ok(new_tls) = connect_to_host(ctx, &host).await {
                                tls = new_tls;
                                break;
                            }
                        }
                    }
                }
            }
        }
    };
    progress.report(3, 3, "done").await;

    let output_summary: String = result.output.chars().take(500).collect();
    super::audit(
        ctx,
        AuditAction::Exec,
        host_name,
        session_name,
        Some(&pane_id),
        command,
        Some(&output_summary),
        result.ok,
        result.duration_ms,
        result.error.as_deref(),
    )
    .await;

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
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    Ok(response)
}

pub(crate) async fn collect_until_exit(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().unwrap_or("clum");
    let pane_id_arg = args["pane_id"].as_str();
    let max_bytes = args["max_bytes"].as_u64().unwrap_or(1048576);
    let timeout_ms = args["timeout_ms"]
        .as_u64()
        .unwrap_or(DEFAULT_COLLECT_TIMEOUT_MS);
    let starting_at = args["starting_at"].as_str().unwrap_or("now");
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let mut tls = connect_to_host(ctx, &host).await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;
    send_json_frame(
        &mut tls,
        &json!({
            "type": "collect_until_exit",
            "session_name": session_name,
            "pane_id": pane_id,
            "max_bytes": max_bytes,
            "timeout_ms": timeout_ms,
            "starting_at": starting_at,
        }),
    )
    .await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::CollectUntilExit,
        host_name,
        session_name,
        Some(&pane_id),
        "",
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // ── 测试辅助 ─────────────────────────────────────────────────

    fn sent_cmd(timeout_ms: u64) -> SentCommand {
        SentCommand {
            precheck_state: None,
            start_marker: "[abc]".to_string(),
            sentinel_marker: "[abc ".to_string(),
            timeout_ms,
            started_at: std::time::Instant::now(),
        }
    }

    /// 用一个后台 task 模拟 bridge：按请求 type 返回 responder 给出的响应。
    /// client 端 drop 时 server 读到 EOF 自动退出。
    async fn mock_bridge<R>(responder: R) -> tokio::io::DuplexStream
    where
        R: Fn(&serde_json::Value) -> serde_json::Value + Send + 'static,
    {
        let (client, mut server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            loop {
                let req = match recv_json_frame(&mut server).await {
                    Ok(r) => r,
                    Err(_) => break,
                };
                let resp = responder(&req);
                if send_json_frame(&mut server, &resp).await.is_err() {
                    break;
                }
            }
        });
        client
    }

    fn assert_refused(outcome: SendOutcome, expected_state: &str, error_hint: &str) {
        match outcome {
            SendOutcome::Done(r) => {
                assert!(r.refused, "terminal_state={expected_state} 应 refused");
                let err = r.error.unwrap_or_default();
                assert!(
                    err.contains(error_hint),
                    "错误建议应包含 '{error_hint}'，实际: {err}"
                );
                assert!(!r.ok);
                assert_eq!(r.exit_code, None);
            }
            SendOutcome::Sent(_) => {
                panic!("terminal_state={expected_state} 不应发送命令")
            }
        }
    }

    // ── unescape_keys ────────────────────────────────────────────

    #[test]
    fn unescape_keys_handles_simple_escapes() {
        assert_eq!(unescape_keys(r"a\nb"), "a\nb");
        assert_eq!(unescape_keys(r"a\tb"), "a\tb");
        assert_eq!(unescape_keys(r"a\rb"), "a\rb");
        assert_eq!(unescape_keys(r"\e"), "\x1b");
    }

    #[test]
    fn unescape_keys_handles_hex_escapes() {
        assert_eq!(unescape_keys(r"\x1b"), "\x1b");
        assert_eq!(unescape_keys(r"\x41"), "A");
        assert_eq!(unescape_keys(r"abc\x00def"), "abc\x00def");
    }

    #[test]
    fn unescape_keys_preserves_invalid_hex() {
        // 非两位 hex 应保留字面量，不吞字符也不注入 NUL
        assert_eq!(unescape_keys(r"\xzz"), r"\xzz");
        assert_eq!(unescape_keys(r"\x4"), r"\x4");
        assert_eq!(unescape_keys(r"\x"), r"\x");
        assert_eq!(unescape_keys(r"\x4z"), r"\x4z");
    }

    #[test]
    fn unescape_keys_preserves_unknown_escapes() {
        assert_eq!(unescape_keys(r"\q"), r"\q");
        assert_eq!(unescape_keys(r"\\"), "\\");
        assert_eq!(unescape_keys(r"abc\"), r"abc\");
        assert_eq!(unescape_keys(r"hello"), "hello");
    }

    // ── parse_exit_code ──────────────────────────────────────────

    #[test]
    fn parse_exit_code_reads_digits_after_sentinel() {
        assert_eq!(parse_exit_code("[abc 0]", "[abc "), Some(0));
        assert_eq!(parse_exit_code("[abc 127]", "[abc "), Some(127));
        assert_eq!(parse_exit_code("line1\n[abc 42]", "[abc "), Some(42));
    }

    #[test]
    fn parse_exit_code_ignores_unexpanded_echo() {
        // echo 回显中 "$?" 尚未展开为数字 —— 必须返回 None
        assert_eq!(parse_exit_code("echo \"[abc $?]\"", "[abc "), None);
    }

    #[test]
    fn parse_exit_code_none_when_absent() {
        assert_eq!(parse_exit_code("no marker here", "[abc "), None);
        assert_eq!(parse_exit_code("[abc ]", "[abc "), None);
        assert_eq!(parse_exit_code("[abc abc]", "[abc "), None);
    }

    // ── parse_exec_output ────────────────────────────────────────

    fn sample_output() -> String {
        [
            "user@host:~$ echo '[abc]'",
            "[abc]",
            "user@host:~$ ls",
            "file1",
            "file2",
            "user@host:~$ echo \"[abc 0]\"",
            "[abc 0]",
        ]
        .join("\n")
    }

    #[test]
    fn parse_exec_output_extracts_command_region() {
        let sent = sent_cmd(5000);
        let r = parse_exec_output(sample_output(), None, None, &sent, 200);
        assert!(r.ok);
        assert_eq!(r.exit_code, Some(0));
        let out = r.output;
        assert!(out.contains("file1"), "输出应含命令结果，实际: {out:?}");
        assert!(out.contains("file2"));
        assert!(
            out.contains("echo \"[abc 0]\""),
            "sentinel 的 echo 命令行应保留在区间内: {out:?}"
        );
        assert!(
            !out.contains("\n[abc 0]"),
            "sentinel 输出行应被排除: {out:?}"
        );
        assert!(
            !out.contains("echo '[abc]'"),
            "start_marker 命令行应被排除: {out:?}"
        );
    }

    #[test]
    fn parse_exec_output_nonzero_exit_code() {
        let text = [
            "user@host:~$ echo '[abc]'",
            "[abc]",
            "user@host:~$ false",
            "user@host:~$ echo \"[abc 1]\"",
            "[abc 1]",
        ]
        .join("\n");
        let sent = sent_cmd(5000);
        let r = parse_exec_output(text, None, None, &sent, 200);
        assert!(!r.ok);
        assert_eq!(r.exit_code, Some(1));
        assert_eq!(r.error, None);
    }

    #[test]
    fn parse_exec_output_truncates_to_max_lines() {
        let mut lines = vec!["user@host:~$ echo '[abc]'".to_string(), "[abc]".to_string()];
        for i in 0..20 {
            lines.push(format!("user@host:~$ echo \"line {i}\""));
            lines.push(format!("line {i}"));
        }
        lines.push("user@host:~$ echo \"[abc 0]\"".to_string());
        lines.push("[abc 0]".to_string());
        let sent = sent_cmd(5000);
        let r = parse_exec_output(lines.join("\n"), None, None, &sent, 5);
        assert_eq!(r.exit_code, Some(0));
        let out_lines: Vec<&str> = r.output.lines().collect();
        assert!(
            out_lines.len() <= 5,
            "max_lines=5 应截断，实际 {} 行",
            out_lines.len()
        );
        assert!(
            out_lines.iter().any(|l| l.contains("line 19")),
            "应保留尾部最新行（line 19），实际: {:?}",
            out_lines
        );
    }

    #[test]
    fn parse_exec_output_sentinel_not_found() {
        let sent = sent_cmd(5000);
        let r = parse_exec_output("some random output".to_string(), None, None, &sent, 200);
        assert!(!r.ok);
        assert_eq!(r.exit_code, None);
        assert!(
            r.error.unwrap().contains("sentinel not found"),
            "应报 sentinel not found"
        );
    }

    #[test]
    fn parse_exec_output_falls_back_when_start_marker_scrolled_away() {
        // start_marker 滚出 scrollback，只有 sentinel —— 回退到从头截取
        let text = ["only-output", "user@host:~$ echo \"[abc 0]\"", "[abc 0]"].join("\n");
        let sent = sent_cmd(5000);
        let r = parse_exec_output(text, None, None, &sent, 200);
        assert_eq!(r.exit_code, Some(0));
        assert!(r.output.contains("only-output"), "应回退到全文本");
    }

    // ── timeout_exec_result ──────────────────────────────────────

    #[test]
    fn timeout_exec_result_formats_message() {
        let sent = sent_cmd(1000);
        let r = timeout_exec_result(&sent, "partial".to_string(), None, None, None);
        assert!(!r.ok);
        assert_eq!(r.exit_code, None);
        let err = r.error.unwrap();
        assert!(err.contains("timeout"), "错误消息应含 timeout: {err}");
        assert!(err.contains("1000"), "应含超时毫秒数: {err}");
        assert_eq!(r.output, "partial");
    }

    #[test]
    fn timeout_exec_result_with_reason() {
        let sent = sent_cmd(1000);
        let r = timeout_exec_result(&sent, String::new(), None, None, Some("boom"));
        let err = r.error.unwrap();
        assert!(err.contains("boom"), "应含失败原因: {err}");
    }

    // ── exec_send：terminal_state 安全检查 ───────────────────────

    #[tokio::test]
    async fn exec_send_refuses_when_terminal_not_ready() {
        let mut stream = mock_bridge(|req| match req["type"].as_str() {
            Some("capture_pane") => json!({"ok": true, "text": "", "terminal_state": "editor"}),
            other => json!({"ok": false, "error": format!("unexpected: {other:?}")}),
        })
        .await;
        let outcome = exec_send(&mut stream, "clum", "%0", "ls", 5000).await;
        assert_refused(outcome, "editor", "editor");
    }

    #[tokio::test]
    async fn exec_send_refuses_each_non_ready_state() {
        let cases = [
            ("pager", "pager"),
            ("password", "password"),
            ("confirm", "confirm"),
            ("running", "wait_stable"),
            ("repl", "REPL"),
            ("unknown", "unknown"),
        ];
        for (state, hint) in cases {
            let mut stream = mock_bridge(move |req| match req["type"].as_str() {
                Some("capture_pane") => {
                    json!({"ok": true, "text": "", "terminal_state": state})
                }
                _ => json!({"ok": false, "error": "unexpected"}),
            })
            .await;
            let outcome = exec_send(&mut stream, "clum", "%0", "ls", 5000).await;
            assert_refused(outcome, state, hint);
        }
    }

    #[tokio::test]
    async fn exec_send_refused_result_carries_pre_terminal_state() {
        let mut stream =
            mock_bridge(|_| json!({"ok": true, "text": "", "terminal_state": "running"})).await;
        let outcome = exec_send(&mut stream, "clum", "%0", "ls", 5000).await;
        match outcome {
            SendOutcome::Done(r) => {
                assert_eq!(r.pre_terminal_state.unwrap(), json!("running"));
                assert_eq!(r.terminal_state.unwrap(), json!("running"));
            }
            SendOutcome::Sent(_) => panic!("should refuse"),
        }
    }

    #[tokio::test]
    async fn exec_send_sends_marker_command_when_ready() {
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let mut stream = mock_bridge(move |req| match req["type"].as_str() {
            Some("capture_pane") => json!({"ok": true, "text": "", "terminal_state": "ready"}),
            Some("send_keys") => {
                sink.lock()
                    .unwrap()
                    .push(req["keys"].as_str().unwrap_or("").to_string());
                json!({"ok": true})
            }
            _ => json!({"ok": false, "error": "unexpected"}),
        })
        .await;
        let outcome = exec_send(&mut stream, "clum", "%0", "ls -la", 5000).await;
        match outcome {
            SendOutcome::Sent(sent) => {
                assert_eq!(sent.timeout_ms, 5000);
                assert!(sent.start_marker.starts_with('['));
                assert!(sent.sentinel_marker.starts_with("["));
            }
            SendOutcome::Done(r) => {
                panic!(
                    "ready 状态应发送命令，实际 refused={} err={:?}",
                    r.refused, r.error
                )
            }
        }
        let keys = captured.lock().unwrap().first().unwrap().clone();
        assert!(keys.contains("ls -la"), "keys 应含命令: {keys:?}");
        assert!(keys.contains("echo '["), "keys 应含 start marker: {keys:?}");
        assert!(keys.contains("$?"), "keys 应含 exit code echo: {keys:?}");
    }

    #[tokio::test]
    async fn exec_send_reports_send_keys_failure() {
        let mut stream = mock_bridge(|req| match req["type"].as_str() {
            Some("capture_pane") => json!({"ok": true, "text": "", "terminal_state": "ready"}),
            Some("send_keys") => json!({"ok": false, "error": "pane %0 not found"}),
            _ => json!({"ok": false, "error": "unexpected"}),
        })
        .await;
        let outcome = exec_send(&mut stream, "clum", "%0", "ls", 5000).await;
        match outcome {
            SendOutcome::Done(r) => {
                assert!(!r.refused, "send_keys 失败不是安全检查拒绝");
                let err = r.error.unwrap();
                assert!(
                    err.contains("pane %0 not found"),
                    "应透传 bridge 错误: {err}"
                );
            }
            SendOutcome::Sent(_) => panic!("send_keys 失败不应返回 Sent"),
        }
    }

    #[tokio::test]
    async fn exec_send_allows_when_terminal_state_missing() {
        // capture_pane 未返回 terminal_state 时向后兼容放行（is_ready 默认 true）
        let mut stream = mock_bridge(|req| match req["type"].as_str() {
            Some("capture_pane") => json!({"ok": true, "text": ""}),
            Some("send_keys") => json!({"ok": true}),
            _ => json!({"ok": false, "error": "unexpected"}),
        })
        .await;
        let outcome = exec_send(&mut stream, "clum", "%0", "ls", 5000).await;
        assert!(
            matches!(outcome, SendOutcome::Sent(_)),
            "terminal_state 缺失应放行执行"
        );
    }
}
