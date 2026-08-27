use super::ProtocolProxy;
use crate::terminal_state::detect_terminal_state;
use rmux_sdk::{PaneRespawnOptions, ProcessCommandSpec, ProcessSpec, SessionName};
use serde_json::json;
use std::path::PathBuf;

/// 注入按键前的终端上下文：状态 + 提示行。
/// 提示行仅在状态为 password 时返回（供 MCP 侧审计脱敏附加上下文）。
/// 快照失败返回 (None, None)——调用方不据此脱敏。
async fn pane_pre_context(pane: &rmux_sdk::Pane) -> (Option<String>, Option<String>) {
    let snapshot = match pane.snapshot().await {
        Ok(s) => s,
        Err(_) => return (None, None),
    };
    let raw_text = snapshot.visible_text();
    let state = detect_terminal_state(&raw_text, snapshot.cursor.col, snapshot.cursor.visible);
    let state_str = serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(String::from));
    let prompt = if state_str.as_deref() == Some("password") {
        sanitize_prompt_line(&raw_text)
    } else {
        None
    };
    (state_str, prompt)
}

/// 从终端可见文本提取提示行（ANSI 剥离后的最后一个非空行）：
/// 去除控制字符、截断至 120 字符（超出加 `…`）。提示行是不可信数据，只做存储展示。
fn sanitize_prompt_line(raw_text: &str) -> Option<String> {
    let cleaned = ProtocolProxy::clean_text(raw_text, None);
    let line = cleaned.lines().last()?.trim();
    let no_ctrl: String = line.chars().filter(|c| !c.is_control()).collect();
    let no_ctrl = no_ctrl.trim();
    if no_ctrl.is_empty() {
        return None;
    }
    let count = no_ctrl.chars().count();
    let mut out: String = no_ctrl.chars().take(120).collect();
    if count > 120 {
        out.push('…');
    }
    Some(out)
}

/// 把注入前终端上下文附加到响应。成功与失败路径共用：
/// 发送失败时同样附加状态，确保 MCP 侧对失败的发送也能审计脱敏
/// （失败的发送也会写审计记录，此时输入未送达终端，明文入库属纯泄漏）。
fn attach_pre_context(
    resp: &mut serde_json::Value,
    pre_state: Option<String>,
    prompt_line: Option<String>,
) {
    if let Some(state) = pre_state {
        resp["pre_terminal_state"] = json!(state);
    }
    if let Some(prompt) = prompt_line {
        resp["prompt_line"] = json!(prompt);
    }
}

impl ProtocolProxy {
    pub async fn handle_send_keys(
        &self,
        session_name: &str,
        pane_id_str: &str,
        keys: &str,
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
            Ok(pane) => {
                let (pre_state, prompt_line) = pane_pre_context(&pane).await;
                match pane.send_key(keys).await {
                    Ok(()) => {
                        let mut resp = json!({"ok": true});
                        attach_pre_context(&mut resp, pre_state, prompt_line);
                        resp
                    }
                    Err(e) => {
                        let mut resp = json!({"ok": false, "error": e.to_string()});
                        attach_pre_context(&mut resp, pre_state, prompt_line);
                        resp
                    }
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_send_text(
        &self,
        session_name: &str,
        pane_id_str: &str,
        text: &str,
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
            Ok(pane) => {
                let (pre_state, prompt_line) = pane_pre_context(&pane).await;
                match pane.send_text(text).await {
                    Ok(()) => {
                        let mut resp = json!({"ok": true});
                        attach_pre_context(&mut resp, pre_state, prompt_line);
                        resp
                    }
                    Err(e) => {
                        let mut resp = json!({"ok": false, "error": e.to_string()});
                        attach_pre_context(&mut resp, pre_state, prompt_line);
                        resp
                    }
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_broadcast_keys(
        &self,
        session_name: &str,
        pane_ids: &[String],
        keys: &str,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let mut panes = Vec::new();
        for pid_str in pane_ids {
            let pane_id = match Self::parse_pane_id(pid_str) {
                Some(id) => id,
                None => {
                    return json!({"ok": false, "error": format!("invalid pane_id: {}", pid_str)})
                }
            };
            match self.rmux.get_pane_by_id(&sn, pane_id).await {
                Ok(p) => panes.push(p),
                Err(e) => return json!({"ok": false, "error": e.to_string()}),
            }
        }
        let mut pre_state_out: Option<String> = None;
        let mut prompt_out: Option<String> = None;
        for pane in &panes {
            let (state, prompt) = pane_pre_context(pane).await;
            if state.as_deref() == Some("password") {
                pre_state_out = Some("password".to_string());
                prompt_out = prompt;
                break;
            }
        }
        match self
            .rmux
            .broadcast(&panes, rmux_sdk::Input::key(keys))
            .await
        {
            Ok(_result) => {
                let mut resp = json!({"ok": true});
                attach_pre_context(&mut resp, pre_state_out, prompt_out);
                resp
            }
            Err(e) => {
                let mut resp = json!({"ok": false, "error": e.to_string()});
                attach_pre_context(&mut resp, pre_state_out, prompt_out);
                resp
            }
        }
    }

    pub async fn handle_cmd_escape(&self, args: &[String]) -> serde_json::Value {
        match self.rmux.cmd(args).await {
            Ok(result) => json!({
                "ok": true,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "exit_code": result.exit,
            }),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_spawn_command(
        &self,
        session_name: &str,
        pane_id_str: &str,
        command: &str,
        args: &[String],
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
        let full_cmd: Vec<String> = std::iter::once(command.to_string())
            .chain(args.iter().cloned())
            .collect();
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => match pane.spawn(full_cmd).await {
                Ok(_target) => json!({"ok": true, "spawned": true}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_shell_command(
        &self,
        session_name: &str,
        pane_id_str: &str,
        cmd: &str,
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
            Ok(pane) => match pane.shell(cmd).await {
                Ok(_target) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            },
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn handle_respawn_pane(
        &self,
        session_name: &str,
        pane_id_str: &str,
        command: Option<String>,
        args: Option<Vec<String>>,
        shell: Option<bool>,
        cwd: Option<String>,
        env: Option<serde_json::Value>,
        kill: Option<bool>,
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
        match self.rmux.get_pane_by_id(&sn, pane_id).await {
            Ok(pane) => {
                let mut opts = PaneRespawnOptions::default();

                if kill == Some(true) {
                    opts.kill = true;
                }

                if let Some(ref cwd_path) = cwd {
                    opts.start_directory = Some(PathBuf::from(cwd_path));
                }

                opts.keep_alive_on_exit = keep_alive_on_exit;

                if let Some(ref cmd) = command {
                    let env_strings: Option<Vec<String>> = env.as_ref().and_then(|env_map| {
                        env_map.as_object().map(|obj| {
                            obj.iter()
                                .map(|(k, v)| {
                                    let val = v.as_str().unwrap_or("");
                                    format!("{}={}", k, val)
                                })
                                .collect()
                        })
                    });
                    let env_not_empty = !matches!(env_strings.as_deref(), Some([]));
                    opts.process = if shell.unwrap_or(false) {
                        ProcessSpec {
                            process_command: Some(ProcessCommandSpec::Shell(cmd.clone())),
                            environment: env_not_empty.then_some(env_strings).flatten(),
                            ..Default::default()
                        }
                    } else {
                        let argv = std::iter::once(cmd.clone())
                            .chain(args.unwrap_or_default())
                            .collect();
                        ProcessSpec {
                            process_command: Some(ProcessCommandSpec::Argv(argv)),
                            environment: env_not_empty.then_some(env_strings).flatten(),
                            ..Default::default()
                        }
                    };
                }

                match pane.respawn(opts).await {
                    Ok(_target) => json!({"ok": true, "respawned": true}),
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{attach_pre_context, sanitize_prompt_line};
    use serde_json::json;

    #[test]
    fn attach_pre_context_success_response() {
        let mut resp = json!({"ok": true});
        attach_pre_context(
            &mut resp,
            Some("password".to_string()),
            Some("[sudo] password for tddh:".to_string()),
        );
        assert_eq!(resp["pre_terminal_state"], "password");
        assert_eq!(resp["prompt_line"], "[sudo] password for tddh:");
    }

    #[test]
    fn attach_pre_context_error_response_keeps_state() {
        let mut resp = json!({"ok": false, "error": "send failed"});
        attach_pre_context(&mut resp, Some("password".to_string()), None);
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"], "send failed");
        assert_eq!(resp["pre_terminal_state"], "password");
        assert!(resp.get("prompt_line").is_none());
    }

    #[test]
    fn attach_pre_context_none_adds_nothing() {
        let mut resp = json!({"ok": true});
        attach_pre_context(&mut resp, None, None);
        assert!(resp.get("pre_terminal_state").is_none());
        assert!(resp.get("prompt_line").is_none());
    }

    #[test]
    fn extracts_last_non_empty_line() {
        let text = "$ sudo apt update\n[sudo] password for tddh: ";
        assert_eq!(
            sanitize_prompt_line(text),
            Some("[sudo] password for tddh:".to_string())
        );
    }

    #[test]
    fn strips_ansi_sequences() {
        let text = "\x1b[32mPassword:\x1b[0m ";
        assert_eq!(sanitize_prompt_line(text), Some("Password:".to_string()));
    }

    #[test]
    fn truncates_long_lines() {
        let text = "x".repeat(200);
        let out = sanitize_prompt_line(&text).unwrap();
        assert_eq!(out.chars().count(), 121); // 120 字符 + '…'
        assert!(out.ends_with('…'));
    }

    #[test]
    fn removes_control_characters() {
        let text = "Pass\x07word:";
        assert_eq!(sanitize_prompt_line(text), Some("Password:".to_string()));
    }

    #[test]
    fn empty_text_returns_none() {
        assert_eq!(sanitize_prompt_line(""), None);
        assert_eq!(sanitize_prompt_line("\n\n  \n"), None);
    }
}
