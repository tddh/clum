use super::ProtocolProxy;
use crate::terminal_state::detect_terminal_state;
use base64::{engine::general_purpose, Engine as _};
use rmux_sdk::{PaneOutputStart, SessionName};
use serde_json::json;

impl ProtocolProxy {
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_capture_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
        max_lines: Option<usize>,
        ansi: Option<bool>,
        start_line: Option<i64>,
        end_line: Option<i64>,
        join_wrapped: Option<bool>,
        preserve_spaces: Option<bool>,
        alternate: Option<bool>,
        buffer_name: Option<String>,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };

        // Path B: any advanced parameter specified → use PaneCaptureBuilder
        let use_path_b = ansi.is_some()
            || alternate.is_some()
            || start_line.is_some()
            || end_line.is_some()
            || buffer_name.is_some()
            || join_wrapped.is_some()
            || preserve_spaces.is_some();

        if use_path_b {
            let pane = match self.rmux.get_pane_by_id(&sn, pane_id).await {
                Ok(p) => p,
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            };
            let mut builder = pane.capture_pane();
            if let Some(ansi) = ansi {
                builder = builder.escape_ansi(ansi);
            }
            if let Some(sl) = start_line {
                builder = builder.start(sl);
            } else if let Some(ml) = max_lines.filter(|&n| n > 0) {
                // max_lines in Path B maps to start_line = -(max_lines) if start_line not set
                builder = builder.start(-(ml as i64));
            }
            if let Some(el) = end_line {
                builder = builder.end(el);
            }
            if let Some(jw) = join_wrapped {
                builder = builder.join_wrapped(jw);
            }
            if let Some(ps) = preserve_spaces {
                builder = builder.preserve_trailing_spaces(ps);
            }
            if let Some(alt) = alternate {
                builder = builder.alternate(alt);
            }
            if let Some(buf_name) = buffer_name {
                builder = builder.buffer(buf_name);
            }
            match builder.await {
                Ok(capture) => {
                    let text = if ansi.unwrap_or(false) {
                        // ANSI output: base64 encode
                        general_purpose::STANDARD.encode(&capture.stdout)
                    } else {
                        String::from_utf8_lossy(&capture.stdout).to_string()
                    };
                    let mut resp = json!({
                        "ok": true,
                        "text": text,
                        "buffer_written": capture.buffer_name,
                    });
                    if ansi.unwrap_or(false) {
                        resp["encoding"] = json!("base64");
                    } else {
                        resp["encoding"] = json!(null);
                    }
                    // Path B 与 Path A 一致附带终端状态（一次本地 snapshot IPC）
                    if let Ok(snapshot) = pane.snapshot().await {
                        let raw_text = snapshot.visible_text();
                        resp["terminal_state"] = json!(detect_terminal_state(
                            &raw_text,
                            snapshot.cursor.col,
                            snapshot.cursor.visible,
                        ));
                        resp["cursor"] = json!({
                            "row": snapshot.cursor.row,
                            "col": snapshot.cursor.col,
                            "visible": snapshot.cursor.visible,
                        });
                    }
                    resp
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            }
        } else {
            // Path A: existing behavior (snapshot + clean_text + max_lines truncation)
            match self.rmux.get_pane_by_id(&sn, pane_id).await {
                Ok(pane) => match pane.snapshot().await {
                    Ok(snapshot) => {
                        let raw_text = snapshot.visible_text();
                        let state = detect_terminal_state(
                            &raw_text,
                            snapshot.cursor.col,
                            snapshot.cursor.visible,
                        );
                        let full_text = Self::clean_text(&raw_text, None);
                        let text = if max_lines.is_some_and(|n| n > 0) {
                            let n = max_lines.unwrap();
                            let lines: Vec<&str> = full_text.lines().collect();
                            if lines.len() > n {
                                lines[lines.len() - n..].join("\n")
                            } else {
                                full_text
                            }
                        } else {
                            full_text
                        };
                        json!({
                            "ok": true,
                            "text": text,
                            "terminal_state": state,
                            "cursor": {
                                "row": snapshot.cursor.row,
                                "col": snapshot.cursor.col,
                                "visible": snapshot.cursor.visible,
                            }
                        })
                    }
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                },
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            }
        }
    }

    pub async fn handle_wait_for_text(
        &self,
        session_name: &str,
        pane_id_str: &str,
        text: &str,
        timeout_ms: u64,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let pane = match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        // 用 per-operation timeout 的 visible-wait，避免 SDK 默认 5s 超时
        // （V1_DEFAULT_TIMEOUT）先于请求 timeout_ms 触发导致假超时。
        let timeout = std::time::Duration::from_millis(timeout_ms);
        match pane
            .expect_visible_text()
            .to_contain(text)
            .timeout(timeout)
            .await
        {
            Ok(snapshot) => {
                let raw_text = snapshot.visible_text();
                let state =
                    detect_terminal_state(&raw_text, snapshot.cursor.col, snapshot.cursor.visible);
                json!({
                    "ok": true,
                    "found": true,
                    "terminal_state": state,
                    "cursor": {
                        "row": snapshot.cursor.row,
                        "col": snapshot.cursor.col,
                        "visible": snapshot.cursor.visible,
                    }
                })
            }
            Err(e) => {
                if matches!(e, rmux_sdk::RmuxError::WaitTimeout { .. }) {
                    let mut resp = json!({"ok": false, "found": false, "error": format!("timeout waiting for: {}", text)});
                    if let Ok(snapshot) = pane.snapshot().await {
                        let raw_text = snapshot.visible_text();
                        resp["partial_output"] = json!(raw_text);
                        resp["terminal_state"] = json!(detect_terminal_state(
                            &raw_text,
                            snapshot.cursor.col,
                            snapshot.cursor.visible,
                        ));
                        resp["cursor"] = json!({
                            "row": snapshot.cursor.row,
                            "col": snapshot.cursor.col,
                            "visible": snapshot.cursor.visible,
                        });
                    }
                    resp
                } else {
                    json!({"ok": false, "error": e.to_string()})
                }
            }
        }
    }

    pub async fn handle_wait_for_bytes(
        &self,
        session_name: &str,
        pane_id_str: &str,
        bytes_b64: &str,
        only_new: bool,
        _timeout_ms: u64,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let pane = match self.rmux_long.get_pane_by_id(&sn, pane_id).await {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let decoded = match general_purpose::STANDARD.decode(bytes_b64) {
            Ok(v) => v,
            Err(e) => return json!({"ok": false, "error": format!("invalid base64: {}", e)}),
        };

        let result = if only_new {
            if self
                .rmux
                .has_capability("sdk.waits.armed")
                .await
                .unwrap_or(false)
            {
                match pane.wait_for_next(&decoded).await {
                    Ok(armed) => armed.await,
                    Err(e) => Err(e),
                }
            } else {
                pane.wait_for(&decoded).await
            }
        } else {
            pane.wait_for(&decoded).await
        };

        match result {
            Ok(()) => json!({"ok": true, "found": true}),
            Err(e) => {
                let mut resp = json!({"ok": false, "found": false, "error": e.to_string()});
                if let Ok(snapshot) = pane.snapshot().await {
                    let raw_text = snapshot.visible_text();
                    resp["partial_output"] = json!(raw_text);
                    resp["terminal_state"] = json!(detect_terminal_state(
                        &raw_text,
                        snapshot.cursor.col,
                        snapshot.cursor.visible,
                    ));
                    resp["cursor"] = json!({
                        "row": snapshot.cursor.row,
                        "col": snapshot.cursor.col,
                        "visible": snapshot.cursor.visible,
                    });
                }
                resp
            }
        }
    }

    pub async fn handle_wait_exit(
        &self,
        session_name: &str,
        pane_id_str: &str,
        timeout_ms: u64,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let pane = match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        // 自行轮询 pane.info() 而非 pane.wait_exit()：后者受 SDK 默认 5s
        // （V1_DEFAULT_TIMEOUT）限制，超过 5s 的等待会假超时。
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let info = match pane.info().await {
                Ok(i) => i,
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            };
            match info.panes.first() {
                None => return json!({"ok": true, "exited": false}),
                Some(p) => {
                    if let Some(ref state) = p.exit_state {
                        return json!({"ok": true, "exited": true, "exit_code": state.code, "signal": state.signal});
                    }
                    if matches!(p.process, rmux_sdk::PaneProcessState::Exited) {
                        return json!({"ok": true, "exited": false});
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                let mut resp = json!({"ok": false, "error": "timeout waiting for exit"});
                if let Ok(snapshot) = pane.snapshot().await {
                    let raw_text = snapshot.visible_text();
                    resp["partial_output"] = json!(raw_text);
                    resp["terminal_state"] = json!(detect_terminal_state(
                        &raw_text,
                        snapshot.cursor.col,
                        snapshot.cursor.visible,
                    ));
                    resp["cursor"] = json!({
                        "row": snapshot.cursor.row,
                        "col": snapshot.cursor.col,
                        "visible": snapshot.cursor.visible,
                    });
                }
                return resp;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    pub async fn handle_wait_stable(
        &self,
        session_name: &str,
        pane_id_str: &str,
        stable_ms: u64,
        timeout_ms: u64,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let pane = match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };

        if stable_ms == 0 {
            return json!({"ok": false, "error": "stable_ms must be positive (0 is meaningless for stability check)"});
        }

        match pane
            .wait_until_stable_for(std::time::Duration::from_millis(stable_ms))
            .timeout(std::time::Duration::from_millis(timeout_ms))
            .await
        {
            Ok(snapshot) => {
                let raw_text = snapshot.visible_text();
                let state =
                    detect_terminal_state(&raw_text, snapshot.cursor.col, snapshot.cursor.visible);
                json!({
                    "ok": true,
                    "stable": true,
                    "terminal_state": state,
                    "cursor": {
                        "row": snapshot.cursor.row,
                        "col": snapshot.cursor.col,
                        "visible": snapshot.cursor.visible,
                    }
                })
            }
            Err(e) => {
                let mut resp = json!({
                    "ok": false,
                    "stable": false,
                    "error": format!("timeout: pane did not stabilize within {}ms: {}", timeout_ms, e)
                });
                if let Ok(snapshot) = pane.snapshot().await {
                    let raw_text = snapshot.visible_text();
                    resp["partial_output"] = json!(raw_text);
                    resp["terminal_state"] = json!(detect_terminal_state(
                        &raw_text,
                        snapshot.cursor.col,
                        snapshot.cursor.visible,
                    ));
                    resp["cursor"] = json!({
                        "row": snapshot.cursor.row,
                        "col": snapshot.cursor.col,
                        "visible": snapshot.cursor.visible,
                    });
                }
                resp
            }
        }
    }

    pub async fn handle_collect_until_exit(
        &self,
        session_name: &str,
        pane_id_str: &str,
        max_bytes: usize,
        timeout_ms: u64,
        starting_at: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let pane = match self.rmux_long.get_pane_by_id(&sn, pane_id).await {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };

        // Dead-pane fast path: process already exited, no exit event will
        // fire. Grab scrollback + exit state immediately.
        if let Ok(info) = pane.info().await {
            if let Some(p) = info.panes.first() {
                let is_dead = matches!(p.process, rmux_sdk::PaneProcessState::Exited)
                    || p.exit_state.is_some();
                if is_dead {
                    let snapshot = pane.snapshot().await;
                    let text = snapshot
                        .map(|s| s.visible_text().to_string())
                        .unwrap_or_default();
                    let (exit_code, signal, message) = match &p.exit_state {
                        Some(state) => (state.code, state.signal, state.message.clone()),
                        None => (None, None, None),
                    };
                    let mut resp = json!({
                        "ok": true,
                        "output": general_purpose::STANDARD.encode(text.as_bytes()),
                        "collected_bytes": text.len(),
                        "exit_code": exit_code,
                        "signal": signal,
                        "truncated": false,
                        "lagged": false,
                        "missed_events": 0,
                    });
                    if let Some(ref msg) = message {
                        resp["message"] = json!(msg);
                    }
                    return resp;
                }
            }
        }

        let start_kind = if starting_at == "oldest" {
            PaneOutputStart::Oldest
        } else {
            PaneOutputStart::Now
        };
        // Clone before spawn: timeout handler needs pane for snapshot
        let pane_for_timeout = pane.clone();
        let handle = tokio::spawn(async move {
            if matches!(start_kind, PaneOutputStart::Oldest) {
                pane.collect_output_until_exit_starting_at(PaneOutputStart::Oldest, max_bytes)
                    .await
            } else {
                pane.collect_output_until_exit(max_bytes).await
            }
        });
        let abort = handle.abort_handle();
        match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), handle).await {
            Ok(Ok(Ok(output))) => {
                let base64_output = general_purpose::STANDARD.encode(&output.bytes);
                let collected_bytes = output.bytes.len();
                let (exit_code, signal, message) = match &output.exit_state {
                    Some(state) => (state.code, state.signal, state.message.clone()),
                    None => (None, None, None),
                };
                let mut resp = json!({
                    "ok": true,
                    "output": base64_output,
                    "collected_bytes": collected_bytes,
                    "exit_code": exit_code,
                    "signal": signal,
                    "truncated": output.truncated,
                    "lagged": output.lagged,
                    "missed_events": output.missed_events,
                });
                if let Some(ref msg) = message {
                    resp["message"] = json!(msg);
                }
                resp
            }
            Ok(Ok(Err(e))) => json!({"ok": false, "error": e.to_string()}),
            Ok(Err(join_err)) => {
                json!({"ok": false, "error": format!("join error: {}", join_err)})
            }
            Err(_) => {
                abort.abort();
                let mut resp =
                    json!({"ok": false, "error": format!("timeout after {}ms", timeout_ms)});
                if let Ok(snapshot) = pane_for_timeout.snapshot().await {
                    let raw_text = snapshot.visible_text();
                    resp["partial_output"] = json!(raw_text);
                    resp["terminal_state"] = json!(detect_terminal_state(
                        &raw_text,
                        snapshot.cursor.col,
                        snapshot.cursor.visible,
                    ));
                    resp["cursor"] = json!({
                        "row": snapshot.cursor.row,
                        "col": snapshot.cursor.col,
                        "visible": snapshot.cursor.visible,
                    });
                }
                resp
            }
        }
    }

    pub async fn handle_find_text_all(
        &self,
        session_name: &str,
        pane_id_str: &str,
        pattern: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.find_text_all(pattern).await {
                Ok(matches) => {
                    let list: Vec<serde_json::Value> = matches
                        .iter()
                        .map(|m| {
                            json!({
                                "start_row": m.start_row,
                                "start_col": m.start_col,
                                "end_row": m.end_row,
                                "end_col": m.end_col,
                                "text": m.text,
                            })
                        })
                        .collect();
                    json!({"ok": true, "matches": list, "count": list.len()})
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }
}
