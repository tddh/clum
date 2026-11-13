use super::ProtocolProxy;
use rmux_sdk::SessionName;
use serde_json::json;

impl ProtocolProxy {
    pub async fn handle_split_window(
        &self,
        session_name_str: &str,
        _dir: &str,
    ) -> serde_json::Value {
        let session_name = match SessionName::new(session_name_str) {
            Ok(n) => n,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(session_name).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        match session.new_window().await {
            Ok(window) => {
                let window_index = window.target().window_index;
                let pane_id = match window.panes().await {
                    Ok(panes) => panes.first().map(|p| format!("{}", p.id)),
                    Err(e) => {
                        tracing::warn!(error = %e, window_index, "split_window: pane listing failed; returning pane_id=null");
                        None
                    }
                };
                Self::split_window_response(window_index, pane_id)
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// split_window 成功响应契约：window_index + pane_id 让调用方直接寻址新窗口。
    /// pane 枚举失败只降级 pane_id 为 null——窗口已创建，整体报失败会误导
    /// 调用方重试、创建重复窗口。
    fn split_window_response(window_index: u32, pane_id: Option<String>) -> serde_json::Value {
        json!({
            "ok": true,
            "window_created": true,
            "window_index": window_index,
            "pane_id": pane_id,
        })
    }

    pub async fn handle_close_window(
        &self,
        session_name: &str,
        window_index: u32,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.close().await {
            Ok(_outcome) => json!({"ok": true, "closed": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_rename_window(
        &self,
        session_name: &str,
        window_index: u32,
        name: &str,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.rename(name).await {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_resize_window(
        &self,
        session_name: &str,
        window_index: u32,
        width: Option<u16>,
        height: Option<u16>,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.resize(width, height).await {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// 解析 pane 所在窗口的索引。attach 时解析一次并缓存：SDK 不公开 pane→window
    /// 的映射，只能用 `display-message` 取（会 spawn 一次 rmux 进程）。
    pub async fn window_index_of_pane(&self, pane_id: &str) -> Option<u32> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane_id.to_string(),
            "#{window_index}".to_string(),
        ];
        match self.rmux.cmd(args).await {
            Ok(run) if run.exit.unwrap_or(0) == 0 => {
                String::from_utf8_lossy(&run.stdout).trim().parse().ok()
            }
            _ => None,
        }
    }

    /// 把窗口尺寸显式设为 (cols, rows)。
    ///
    /// 必须走 resize-window 而不是 resize-pane：会话开启 `status` 时窗口高度上限
    /// 被压到「客户端行数 − 1」，只 resize-pane 会被钳在窗口内，pane 比本地终端
    /// 少一行，光标停在倒数第二行（raw 直通没有 rmux 客户端，状态栏本不渲染，
    /// 却仍限制窗口高度）。
    pub async fn resize_window_sized(
        &self,
        session_name: &str,
        window_index: u32,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        let sn = SessionName::new(session_name).map_err(|e| e.to_string())?;
        let session = self.rmux.session(sn).await.map_err(|e| e.to_string())?;
        session
            .window(window_index)
            .resize(Some(cols), Some(rows))
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn handle_select_window(
        &self,
        session_name: &str,
        window_index: u32,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.select().await {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_select_layout(
        &self,
        session_name: &str,
        window_index: u32,
        layout: &str,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        let layout_name = match layout {
            "even-horizontal" => rmux_sdk::LayoutName::EvenHorizontal,
            "even-vertical" => rmux_sdk::LayoutName::EvenVertical,
            "main-horizontal" => rmux_sdk::LayoutName::MainHorizontal,
            "main-vertical" => rmux_sdk::LayoutName::MainVertical,
            "tiled" => rmux_sdk::LayoutName::Tiled,
            _ => {
                return json!({"ok": false, "error": format!("unknown layout: {}. Use: even-horizontal, even-vertical, main-horizontal, main-vertical, tiled", layout)})
            }
        };
        match window.select_layout(layout_name).await {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_window_info(
        &self,
        session_name: &str,
        window_index: u32,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.info().await {
            Ok(info) => {
                let win_info = info.windows.iter().find(|w| w.index == window_index);
                match win_info {
                    Some(w) => json!({
                        "ok": true,
                        "info": {
                            "window_id": format!("{}", w.id),
                            "size_cols": w.size.cols,
                            "size_rows": w.size.rows,
                            "name": w.name,
                            "index": w.index,
                        }
                    }),
                    None => json!({"ok": false, "error": "window not found in info snapshot"}),
                }
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    pub async fn handle_list_window_panes(
        &self,
        session_name: &str,
        window_index: u32,
    ) -> serde_json::Value {
        let sn = match SessionName::new(session_name) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let session = match self.rmux.session(sn).await {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        let window = session.window(window_index);
        match window.panes().await {
            Ok(panes) => {
                let list: Vec<serde_json::Value> = panes
                    .iter()
                    .map(|wp| {
                        json!({
                            "pane_id": format!("{}", wp.id),
                            "active": wp.active,
                        })
                    })
                    .collect();
                json!({"ok": true, "panes": list})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_window_response_carries_addressing_fields() {
        // 响应契约：调用方依赖 window_index + pane_id 直接寻址新窗口，
        // 字段缺失/改名会破坏 SKILL/TOOLS 的临时窗口工作流。
        let resp = ProtocolProxy::split_window_response(2, Some("%7".to_string()));
        assert_eq!(resp["ok"], json!(true));
        assert_eq!(resp["window_created"], json!(true));
        assert_eq!(resp["window_index"], json!(2));
        assert_eq!(resp["pane_id"], json!("%7"));
    }

    #[test]
    fn split_window_response_pane_lookup_failure_does_not_flip_ok() {
        // 窗口已创建成功：pane 枚举失败只降级 pane_id 为 null，
        // 整体报失败会误导调用方重试、创建重复窗口。
        let resp = ProtocolProxy::split_window_response(2, None);
        assert_eq!(resp["ok"], json!(true));
        assert_eq!(resp["window_index"], json!(2));
        assert!(resp["pane_id"].is_null());
    }
}
