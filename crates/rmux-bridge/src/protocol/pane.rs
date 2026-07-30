use super::ProtocolProxy;
use crate::terminal_state::detect_terminal_state;
use anyhow::Result;
use regex::Regex;
use rmux_sdk::{
    capture::{CapturedRegion, Rect},
    PaneCloseOutcome, PaneOutputChunk, PaneProcessState, SessionName, SplitDirection,
    TerminalSizeSpec,
};
use serde_json::json;
use std::path::PathBuf;
use tokio::sync::mpsc;

impl ProtocolProxy {
    pub async fn handle_attach_session(&self, name: &str) -> serde_json::Value {
        let session_name = match SessionName::new(name) {
            Ok(n) => n,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.has_session(session_name).await {
            Ok(true) => json!({"ok": true, "session_name": name, "attached": true}),
            Ok(false) => json!({"ok": false, "error": format!("session not found: {}", name)}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_detach_session(&self, name: &str) -> serde_json::Value {
        let session_name = match SessionName::new(name) {
            Ok(n) => n,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match self.rmux.has_session(session_name).await {
            Ok(true) => json!({"ok": true, "session_name": name, "detached": true}),
            Ok(false) => json!({"ok": false, "error": format!("session not found: {}", name)}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn subscribe_pane_output(
        &self,
        session_name: &str,
        pane_id_str: &str,
    ) -> Result<super::PaneOutputStream> {
        let pane_id = Self::parse_pane_id(pane_id_str)
            .ok_or_else(|| anyhow::anyhow!("invalid pane_id: {}", pane_id_str))?;
        let sn = SessionName::new(session_name).map_err(|e| anyhow::anyhow!("{}", e))?;
        let pane = self.rmux.get_pane_by_id(&sn, pane_id).await?;
        let snapshot = pane.snapshot().await?;
        let (tx, rx) = mpsc::channel(256);
        let _ = tx.send(snapshot.visible_text()).await;
        let mut stream = pane.output_stream().await?;
        tokio::spawn(async move {
            while let Ok(Some(chunk)) = stream.next().await {
                let text = match chunk {
                    PaneOutputChunk::Bytes { bytes, .. } => {
                        String::from_utf8_lossy(&bytes).to_string()
                    }
                    PaneOutputChunk::Lag(_) => continue,
                    _ => continue,
                };
                if tx.send(text).await.is_err() {
                    break;
                }
            }
        });
        Ok(super::PaneOutputStream { rx })
    }

    pub async fn handle_split_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
        direction: &str,
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
        let dir = match direction {
            "horizontal" | "h" => SplitDirection::Down,
            "vertical" | "v" => SplitDirection::Right,
            _ => {
                return json!({"ok": false, "error": format!("invalid direction: {}. Use horizontal or vertical", direction)})
            }
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.split(dir).await {
                Ok(new_pane) => match new_pane.id().await {
                    Ok(Some(id)) => json!({"ok": true, "pane_id": format!("{}", id)}),
                    Ok(None) => json!({"ok": false, "error": "split pane has no id"}),
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                },
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_resize_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
        cols: u16,
        rows: u16,
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
            Ok(pane) => match pane.resize(TerminalSizeSpec::new(cols, rows)).await {
                Ok(()) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_set_pane_title(
        &self,
        session_name: &str,
        pane_id_str: &str,
        title: &str,
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
            Ok(pane) => match pane.set_title(title).await {
                Ok(()) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_get_pane_title(
        &self,
        session_name: &str,
        pane_id_str: &str,
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
            Ok(pane) => match pane.title().await {
                Ok(title) => {
                    json!({"ok": true, "pane_id": pane_id_str, "title": title})
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_close_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
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
            Ok(pane) => match pane.close().await {
                Ok(outcome) => {
                    let closed = matches!(outcome, PaneCloseOutcome::Closed { .. });
                    json!({"ok": true, "closed": closed})
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_pane_info(
        &self,
        session_name: &str,
        pane_id_str: &str,
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
            Ok(pane) => match pane.info().await {
                Ok(info) => {
                    let pane_info = info.panes.iter().find(|p| p.id == pane_id);
                    let (terminal_state, cursor_info) = match pane.snapshot().await {
                        Ok(snapshot) => {
                            let raw_text = snapshot.visible_text();
                            let state = detect_terminal_state(
                                &raw_text,
                                snapshot.cursor.col,
                                snapshot.cursor.visible,
                            );
                            (
                                serde_json::to_value(state).unwrap_or(serde_json::Value::Null),
                                json!({
                                    "row": snapshot.cursor.row,
                                    "col": snapshot.cursor.col,
                                    "visible": snapshot.cursor.visible,
                                }),
                            )
                        }
                        Err(_) => (serde_json::Value::Null, serde_json::Value::Null),
                    };
                    match pane_info {
                        Some(p) => json!({
                            "ok": true,
                            "info": {
                                "pane_id": format!("{}", p.id),
                                "window_id": format!("{}", p.window_id),
                                "session_id": format!("{}", p.session_id),
                                "index": p.index,
                                "size_cols": p.size.cols,
                                "size_rows": p.size.rows,
                                "command": p.command,
                                "working_directory": p.working_directory,
                                "tags": p.tags,
                            },
                            "terminal_state": terminal_state,
                            "cursor": cursor_info,
                        }),
                        None => json!({"ok": false, "error": "pane not found in info snapshot"}),
                    }
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_pane_exists(
        &self,
        session_name: &str,
        pane_id_str: &str,
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
            Ok(pane) => match pane.exists().await {
                Ok(exists) => json!({"ok": true, "exists": exists}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_find_panes(&self, args: &serde_json::Value) -> serde_json::Value {
        let mut finder = self.rmux.find_panes();

        if let Some(s) = args["session_name"].as_str() {
            finder = finder.session(s);
        }
        if let Some(t) = args["title"].as_str() {
            finder = finder.title(t);
        }
        if let Some(p) = args["title_prefix"].as_str() {
            finder = finder.title_prefix(p);
        }
        if let Some(c) = args["command_contains"].as_str() {
            finder = finder.command_contains(c);
        }
        if let Some(d) = args["cwd_contains"].as_str() {
            finder = finder.cwd_contains(d);
        }
        if let Some(idx) = args["window_index"].as_u64() {
            finder = finder.window_index(idx as u32);
        }
        let running = args["running"].as_bool().unwrap_or(false);
        let exited = args["exited"].as_bool().unwrap_or(false);

        if running && exited {
            return json!({"ok": false, "error": "running and exited are mutually exclusive"});
        }
        if running {
            finder = finder.running();
        }
        if exited {
            finder = finder.exited();
        }

        match finder.all().await {
            Ok(panes) => {
                let list: Vec<serde_json::Value> = panes
                    .iter()
                    .map(|d| {
                        let (process_str, pid): (String, Option<u32>) = match &d.process {
                            PaneProcessState::Unknown => ("unknown".to_string(), None),
                            PaneProcessState::Running { pid } => ("running".to_string(), *pid),
                            PaneProcessState::Exited => ("exited".to_string(), None),
                            _ => ("unknown".to_string(), None),
                        };
                        let mut obj = json!({
                            "pane_id": format!("{}", d.pane_id),
                            "session_name": d.session_name.to_string(),
                            "session_id": format!("{}", d.session_id),
                            "window_id": format!("{}", d.window_id),
                            "window_index": d.window_index,
                            "pane_index": d.pane_index,
                            "title": d.title,
                            "command": d.command,
                            "working_directory": d.working_directory,
                            "process": process_str,
                            "tags": d.tags,
                        });
                        if let Some(p) = pid {
                            obj["pid"] = json!(p);
                        }
                        obj
                    })
                    .collect();
                json!({"ok": true, "panes": list, "count": list.len()})
            }
            Err(e) => {
                tracing::warn!("find_panes SDK failed, falling back to CLI: {e}");
                self.find_panes_cli_fallback(args).await
            }
        }
    }

    async fn find_panes_cli_fallback(&self, args: &serde_json::Value) -> serde_json::Value {
        let socket = self.socket_path();
        let session_filter = args["session_name"].as_str();
        let title_filter = args["title"].as_str();

        let mut cmd_args = vec!["-S", socket, "list-panes", "-s"];
        let target_val;
        if let Some(s) = session_filter {
            cmd_args.push("-t");
            target_val = s.to_string();
            cmd_args.push(&target_val);
        }
        cmd_args.push("-F");
        cmd_args.push("#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_id}\t#{pane_title}\t#{pane_current_command}\t#{pane_dead}\t#{pane_pid}\t#{pane_current_path}");

        let output = match tokio::process::Command::new("rmux")
            .args(&cmd_args)
            .output()
            .await
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            Ok(o) => return json!({"ok": false, "error": format!("rmux CLI failed: {}", String::from_utf8_lossy(&o.stderr))}),
            Err(e) => return json!({"ok": false, "error": format!("rmux CLI spawn failed: {e}")}),
        };

        let mut panes: Vec<serde_json::Value> = Vec::new();
        for line in output.lines() {
            let parts: Vec<&str> = line.splitn(9, '\t').collect();
            if parts.len() < 9 { continue; }
            let title = parts[4];
            if let Some(f) = title_filter {
                if title != f { continue; }
            }
            let dead = parts[6] == "1";
            panes.push(json!({
                "pane_id": parts[3],
                "session_name": parts[0],
                "window_index": parts[1].parse::<u32>().unwrap_or(0),
                "pane_index": parts[2].parse::<u32>().unwrap_or(0),
                "title": title,
                "command": parts[5],
                "working_directory": parts[8],
                "process": if dead { "exited" } else { "running" },
                "pid": parts[7].parse::<u32>().ok(),
            }));
        }
        json!({"ok": true, "panes": panes, "count": panes.len(), "fallback": true})
    }

    pub async fn handle_find_pane_text(
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
            Ok(pane) => match pane.find_text(pattern).await {
                Ok(Some(m)) => json!({
                    "ok": true,
                    "found": true,
                    "match": {
                        "start_row": m.start_row, "start_col": m.start_col,
                        "end_row": m.end_row, "end_col": m.end_col,
                        "text": m.text,
                    }
                }),
                Ok(None) => json!({"ok": true, "found": false}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_clear_history(
        &self,
        _session_name: &str,
        pane_id_str: &str,
    ) -> serde_json::Value {
        let pane_id = match Self::parse_pane_id(pane_id_str) {
            Some(id) => id,
            None => {
                return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)})
            }
        };
        match self
            .rmux
            .cmd(&["clear-history", "-t", &pane_id.to_string()])
            .await
        {
            Ok(result) => {
                if result.exit == Some(0) {
                    json!({"ok": true})
                } else {
                    let code = result
                        .exit
                        .map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'clear-history' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn handle_split_pane_with(
        &self,
        session_name: &str,
        pane_id_str: &str,
        direction: &str,
        command: &str,
        args: &[String],
        shell: bool,
        cwd: Option<String>,
        env: Option<serde_json::Value>,
        title: Option<String>,
        keep_alive_on_exit: Option<bool>,
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
        let dir = match direction {
            "horizontal" | "h" => SplitDirection::Down,
            "vertical" | "v" => SplitDirection::Right,
            _ => {
                return json!({"ok": false, "error": format!("invalid direction: {}. Use horizontal or vertical", direction)})
            }
        };
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => {
                let builder = pane.split_with(dir);
                let mut cmd_builder = if shell {
                    builder.shell(command)
                } else {
                    let argv: Vec<String> = std::iter::once(command.to_string())
                        .chain(args.iter().cloned())
                        .collect();
                    builder.spawn(argv)
                };
                if let Some(ref cwd_path) = cwd {
                    cmd_builder = cmd_builder.cwd(PathBuf::from(cwd_path));
                }
                if let Some(ref env_map) = env {
                    if let Some(obj) = env_map.as_object() {
                        for (k, v) in obj {
                            if let Some(val_str) = v.as_str() {
                                cmd_builder = cmd_builder.env(k, val_str);
                            }
                        }
                    }
                }
                if let Some(ref t) = title {
                    cmd_builder = cmd_builder.title(t);
                }
                if let Some(keep) = keep_alive_on_exit {
                    cmd_builder = cmd_builder.keep_alive_on_exit(keep);
                }
                match cmd_builder.await {
                    Ok(new_pane) => match new_pane.id().await {
                        Ok(Some(id)) => json!({"ok": true, "new_pane_id": format!("{}", id)}),
                        Ok(None) => json!({"ok": false, "error": "split pane has no id"}),
                        Err(e) => json!({"ok": false, "found": true, "error": e.to_string()}),
                    },
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_get_pane_by_title(&self, title: &str) -> serde_json::Value {
        match self.rmux.get_pane_by_title(title).await {
            Ok(pane) => {
                let (pane_title, title_error) = match pane.title().await {
                    Ok(t) => (t.unwrap_or_default(), None),
                    Err(e) => (String::new(), Some(e.to_string())),
                };
                let (pane_id, id_error) = match pane.id().await {
                    Ok(id) => (id, None),
                    Err(e) => (None, Some(e.to_string())),
                };
                match pane.info().await {
                    Ok(info) => {
                        let pane_info = info
                            .panes
                            .iter()
                            .find(|p| pane_id.as_ref().is_some_and(|id| p.id == *id));
                        match pane_info {
                            Some(p) => {
                                let (process_str, pid): (String, Option<u32>) = match &p.process {
                                    PaneProcessState::Unknown => ("unknown".to_string(), None),
                                    PaneProcessState::Running { pid } => {
                                        ("running".to_string(), *pid)
                                    }
                                    PaneProcessState::Exited => ("exited".to_string(), None),
                                    _ => ("unknown".to_string(), None),
                                };
                                let mut obj = json!({
                                    "pane_id": format!("{}", p.id),
                                    "session_id": format!("{}", p.session_id),
                                    "window_id": format!("{}", p.window_id),
                                    "pane_index": p.index,
                                    "title": pane_title,
                                    "command": p.command,
                                    "working_directory": p.working_directory,
                                    "process": process_str,
                                    "tags": p.tags,
                                });
                                if let Some(p) = pid {
                                    obj["pid"] = json!(p);
                                }
                                let mut resp = json!({"ok": true, "found": true, "pane": obj});
                                if let Some(e) = title_error {
                                    resp["title_error"] = json!(e);
                                }
                                if let Some(e) = id_error {
                                    resp["id_error"] = json!(e);
                                }
                                resp
                            }
                            None => {
                                let mut resp = json!({"ok": false, "found": false, "error": format!("no pane found with title: {}", title)});
                                if let Some(e) = title_error {
                                    resp["title_error"] = json!(e);
                                }
                                if let Some(e) = id_error {
                                    resp["id_error"] = json!(e);
                                }
                                resp
                            }
                        }
                    }
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                }
            }
            Err(e) => {
                tracing::warn!("get_pane_by_title SDK failed, falling back to CLI: {e}");
                let args = json!({"title": title});
                let result = self.find_panes_cli_fallback(&args).await;
                if result["ok"].as_bool().unwrap_or(false) {
                    let panes = result["panes"].as_array();
                    match panes.and_then(|p| p.first()) {
                        Some(pane) => json!({"ok": true, "found": true, "pane": pane, "fallback": true}),
                        None => json!({"ok": true, "found": false, "fallback": true}),
                    }
                } else {
                    result
                }
            }
        }
    }

    pub async fn handle_break_pane(
        &self,
        _session_name: &str,
        pane_id_str: &str,
        destination_window: Option<u32>,
        detached: bool,
    ) -> serde_json::Value {
        if !pane_id_str.is_empty() && Self::parse_pane_id(pane_id_str).is_none() {
            return json!({"ok": false, "error": format!("invalid pane_id: {}", pane_id_str)});
        }
        let win_str = destination_window.map(|dw| format!(":{}", dw));
        let mut args: Vec<String> = vec!["break-pane".to_string()];
        if !pane_id_str.is_empty() {
            args.push("-s".to_string());
            args.push(pane_id_str.to_string());
        }
        if let Some(ref win) = win_str {
            args.push("-t".to_string());
            args.push(win.clone());
        }
        if detached {
            args.push("-d".to_string());
        }
        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        match self.rmux.cmd(&args_refs).await {
            Ok(result) => {
                if result.exit == Some(0) {
                    let re = super::PANE_ID_RE.get_or_init(|| Regex::new(r"%(\d+)").unwrap());
                    let stdout = String::from_utf8_lossy(&result.stdout);
                    let pid_opt = re
                        .find(&stdout)
                        .map(|m| format!("%{}", m.as_str().trim_start_matches('%')));
                    json!({"ok": true, "pane_id": pid_opt.unwrap_or_default(), "window_index": destination_window})
                } else {
                    let code = result
                        .exit
                        .map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'break-pane' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_join_pane(
        &self,
        _session_name: &str,
        source_pane_id: &str,
        target_pane_id: &str,
        direction: Option<&str>,
        size: Option<u32>,
    ) -> serde_json::Value {
        if Self::parse_pane_id(source_pane_id).is_none() {
            return json!({"ok": false, "error": format!("invalid source_pane_id: {}", source_pane_id)});
        }
        if Self::parse_pane_id(target_pane_id).is_none() {
            return json!({"ok": false, "error": format!("invalid target_pane_id: {}", target_pane_id)});
        }
        let size_str = size.map(|s| s.to_string());
        let mut args: Vec<String> = vec![
            "join-pane".to_string(),
            "-s".to_string(),
            source_pane_id.to_string(),
            "-t".to_string(),
            target_pane_id.to_string(),
        ];
        if let Some(d) = direction {
            match d {
                "horizontal" | "h" => args.push("-h".to_string()),
                "vertical" | "v" => args.push("-v".to_string()),
                invalid => {
                    return json!({"ok": false, "error": format!("invalid direction: {}. Expected 'horizontal'/'h' or 'vertical'/'v'", invalid)})
                }
            }
        }
        if let Some(ref s) = size_str {
            args.push("-l".to_string());
            args.push(s.clone());
        }
        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        match self.rmux.cmd(&args_refs).await {
            Ok(result) => {
                if result.exit == Some(0) {
                    json!({"ok": true, "source_pane_id": source_pane_id, "target_pane_id": target_pane_id})
                } else {
                    let code = result
                        .exit
                        .map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'join-pane' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_swap_pane(
        &self,
        _session_name: &str,
        source_pane_id: &str,
        target_pane_id: &str,
        detached: bool,
    ) -> serde_json::Value {
        if Self::parse_pane_id(source_pane_id).is_none() {
            return json!({"ok": false, "error": format!("invalid source_pane_id: {}", source_pane_id)});
        }
        if Self::parse_pane_id(target_pane_id).is_none() {
            return json!({"ok": false, "error": format!("invalid target_pane_id: {}", target_pane_id)});
        }
        let mut args = vec!["swap-pane", "-s", source_pane_id, "-t", target_pane_id];
        if detached {
            args.push("-d");
        }
        match self.rmux.cmd(&args).await {
            Ok(result) => {
                if result.exit == Some(0) {
                    json!({"ok": true, "source_pane_id": source_pane_id, "target_pane_id": target_pane_id})
                } else {
                    let code = result
                        .exit
                        .map_or_else(|| "none".to_string(), |c| c.to_string());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    json!({"ok": false, "error": format!("CLI command 'swap-pane' exited with code {}: {}", code, stderr)})
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_capabilities(&self, check: Option<&str>) -> serde_json::Value {
        match self.rmux.capabilities().await {
            Ok(capabilities) => {
                let mut resp = json!({
                    "ok": true,
                    "capabilities": capabilities,
                    "count": capabilities.len(),
                });
                if let Some(c) = check {
                    match self.rmux.has_capability(c).await {
                        Ok(has) => {
                            resp["has_capability"] = json!(has);
                        }
                        Err(e) => return json!({"ok": false, "error": e.to_string()}),
                    }
                }
                resp
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn handle_capture_region(
        &self,
        session_name: &str,
        pane_id_str: &str,
        row: Option<u16>,
        col: Option<u16>,
        rows: Option<u16>,
        cols: Option<u16>,
        styled: bool,
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

        let result = match (row, col, rows, cols) {
            (Some(r), Some(c), Some(rs), Some(cs)) => {
                if rs == 0 || cs == 0 {
                    return json!({"ok": false, "error": "rows and cols must be non-zero"});
                }
                let rect = Rect {
                    row: r,
                    col: c,
                    rows: rs,
                    cols: cs,
                };
                pane.capture_region(rect).preserve_style(styled).await
            }
            (None, None, None, None) => pane.screenshot().preserve_style(styled).await,
            _ => {
                return json!({
                    "ok": false,
                    "error": "all 4 coordinates required (row, col, rows, cols), or omit all for full screenshot"
                });
            }
        };

        match result {
            Ok(CapturedRegion { text, .. }) => {
                json!({"ok": true, "text": text, "styled": styled})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 获取 rmux pane 对象（供 interactive 模块使用）
    pub async fn get_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
    ) -> anyhow::Result<rmux_sdk::Pane> {
        let pane_id = Self::parse_pane_id(pane_id_str)
            .ok_or_else(|| anyhow::anyhow!("invalid pane_id: {}", pane_id_str))?;
        let sn = SessionName::new(session_name).map_err(|e| anyhow::anyhow!("{}", e))?;
        self.rmux
            .get_pane_by_id(&sn, pane_id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}
