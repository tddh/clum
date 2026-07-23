use super::ProtocolProxy;
use rmux_sdk::{EnsureSession, EnsureSessionPolicy, ProcessCommandSpec, ProcessSpec, SessionName};
use serde_json::json;
use std::ffi::CStr;

fn system_user() -> Option<(String, String, String)> {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        let home = CStr::from_ptr((*pw).pw_dir).to_string_lossy().to_string();
        let user = CStr::from_ptr((*pw).pw_name).to_string_lossy().to_string();
        let shell = CStr::from_ptr((*pw).pw_shell).to_string_lossy().to_string();
        Some((home, user, shell))
    }
}

impl ProtocolProxy {
    pub async fn handle_new_session(&self, name: &str, detached: bool) -> serde_json::Value {
        let session_name = match SessionName::new(name) {
            Ok(n) => n,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };

        let (env_vars, login_cmd) = match system_user() {
            Some((home, user, shell)) => {
                let envs = vec![
                    format!("HOME={}", home),
                    format!("USER={}", user),
                    format!("LOGNAME={}", user),
                    format!("SHELL={}", shell),
                ];
                let cmd = format!("exec {} -l", shell);
                (envs, cmd)
            }
            None => (vec![], String::from("exec /bin/sh -l")),
        };

        let ensure = EnsureSession::named(session_name.clone())
            .policy(EnsureSessionPolicy::CreateOrReuse)
            .detached(detached)
            .process(ProcessSpec {
                environment: Some(env_vars),
                process_command: Some(ProcessCommandSpec::Shell(login_cmd)),
                ..Default::default()
            });

        match self.rmux.ensure_session(ensure).await {
            Ok(session) => {
                let pane = session.pane(0, 0);
                match pane.id().await {
                    Ok(Some(pane_id)) => {
                        json!({
                            "ok": true,
                            "session_name": name,
                            "pane_id": pane_id.to_string(),
                            "window_count": 1,
                        })
                    }
                    Ok(None) => json!({"ok": false, "error": "pane has no id"}),
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_list_sessions(&self) -> serde_json::Value {
        match self.rmux.list_sessions().await {
            Ok(sessions) => {
                let list: Vec<serde_json::Value> = sessions
                    .iter()
                    .map(|s| json!({"session_name": s.to_string()}))
                    .collect();
                json!({"ok": true, "sessions": list})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_kill_session(&self, session_name: &str) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match session.kill().await {
            Ok(killed) => json!({"ok": true, "killed": killed}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_find_sessions(&self, args: &serde_json::Value) -> serde_json::Value {
        let mut finder = self.rmux.find_sessions();

        if let Some(name) = args["name"].as_str() {
            finder = finder.name(name);
        }

        match finder.all().await {
            Ok(sessions) => {
                let list: Vec<serde_json::Value> = sessions
                    .iter()
                    .map(|d| json!({"session_name": d.name.to_string()}))
                    .collect();
                json!({"ok": true, "sessions": list, "count": list.len()})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 获取 rmux session 对象（供 interactive 模块使用）
    pub async fn get_session(&self, name: &str) -> anyhow::Result<rmux_sdk::Session> {
        let sn = SessionName::new(name).map_err(|e| anyhow::anyhow!("{}", e))?;
        self.rmux
            .session(sn)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}
