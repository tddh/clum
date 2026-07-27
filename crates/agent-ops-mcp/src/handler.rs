use crate::tools;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};

pub async fn run_mcp_stdio_loop(
    ctx: Arc<tools::ToolContext>,
    tools_def: Value,
) -> anyhow::Result<()> {
    let stdin = BufReader::new(stdin());
    let mut stdout = stdout();
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = json_rpc_error(None, -32700, &format!("Parse error: {e}"));
                stdout.write_all(err.to_string().as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
                continue;
            }
        };

        let method = request["method"].as_str().unwrap_or("");
        let id = request.get("id").cloned();

        let response = match method {
            "tools/list" => json_rpc_response(id, &tools_def),
            "tools/call" => {
                let tool_name = request["params"]["name"].as_str().unwrap_or("");
                let args = request["params"]["arguments"].clone();
                match tools::execute_tool(&ctx, tool_name, args).await {
                    Ok(mut result) => {
                        crate::error::enrich_error(&mut result);
                        let is_error = result.get("ok").and_then(Value::as_bool) == Some(false);
                        let mut payload = json!({
                            "content": [{ "type": "text", "text": result.to_string() }]
                        });
                        if is_error {
                            payload["isError"] = json!(true);
                        }
                        json_rpc_response(id, &payload)
                    }
                    Err(e) => {
                        // 未知工具是协议层错误（MCP 规范 -32602）；其余业务失败
                        // 统一走 result 信封 + isError，保证错误内容进入模型上下文。
                        if e.to_string().starts_with("unknown tool") {
                            json_rpc_error(id, -32602, &format!("{e:#}"))
                        } else {
                            let result = crate::error::error_result(&e);
                            json_rpc_response(
                                id,
                                &json!({
                                    "content": [{ "type": "text", "text": result.to_string() }],
                                    "isError": true,
                                }),
                            )
                        }
                    }
                }
            }
            "initialize" => {
                let agent_name = request["params"]["clientInfo"]["name"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();
                *ctx.agent_name.lock().unwrap_or_else(|e| e.into_inner()) = agent_name;
                json_rpc_response(
                    id,
                    &json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "agent-ops-mcp", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": "You are an AI agent managing remote Linux hosts via agent-ops.\n\n\
                    ## Core Concepts\n\
                    - agent-ops is a remote operations platform: You → MCP Server → QUIC Bridge → rmux daemon → Linux host. This is NOT direct SSH.\n\
                    - **Sessions run inside rmux (a terminal multiplexer like tmux) and survive disconnects**. You can disconnect and reconnect to the same session. Long-running commands keep running in the background.\n\
                    - Sessions are shared resources: the same session can be used by AI (via MCP) and humans (via CLI `connect`) simultaneously or in turns.\n\
                    - You do NOT hold SSH keys. Security is handled by the Bridge proxy + Token auth + TLS.\n\
                    - Every operation runs inside an existing session's pane — it does NOT open a new SSH connection.\n\
                    - Multiple hosts are managed through a hosts.yaml registry (`host_list` to see available hosts).\n\n\
                    ## Tool Selection Rules\n\
                    1. If the target host is in the agent-ops registry (`host_list`), **prefer agent-ops tools** — they provide audit trails, session persistence, and security management.\n\
                    2. If the target host is **NOT in the registry**, or the user **explicitly asks for SSH/SCP/rsync**, use SSH directly. agent-ops is not a universal tool — it only works with registered hosts.\n\
                    3. Default session name: `\"agent-ops\"`. Always `session_attach` first to check if it exists; `session_create` if not found.\n\
                    4. File transfer: `file_upload` / `file_download` (registered hosts); SSH/SCP (unregistered hosts). Commands: `exec` for one-shot (auto-waits, default 200 lines / 600s=10min timeout, set `max_lines=0` for full output), `send_keys` for interactive programs.\n\
                    5. Use `wait_for_text` to block until specific text appears — do NOT poll `capture_pane` in a loop.\n\
                    6. For long-running commands (tail -f, builds): use `stream_pane` for incremental output instead of polling `capture_pane`.\n\
                    7. On failure (`ok:false`), branch on `error_code` (stable contract) and follow `recovery_hint`; `retryable:false` means never blindly retry (e.g. exec TIMEOUT — the command may still be running remotely).\n\n\
                    ## Basic Workflow\n\
                    `host_list` → `session_attach host=<h> session_name=\"agent-ops\"` (or `session_create`) → `exec`/`send_keys` → `capture_pane`/`wait_for_text`.\n\
                    - `pane_id` is optional for most tools. If omitted, the server auto-detects the first pane in window 0. The response includes `resolved_pane_id` and `auto_resolved: true` when auto-detected.\n\
                    - Destructive tools (`close_pane`, `paste_buffer`, `respawn_pane`) still require explicit `pane_id`.\n\
                    - `exec` supports `clear_screen: true` and `timeout_ms` for long commands.\n\
                    - After closing a pane: `respawn_pane` to restart the shell.\n\
                    - `cmd_escape` for direct rmux CLI access (advanced)."
                    }),
                )
            }
            _ => json_rpc_error(id, -32601, &format!("Method not found: {method}")),
        };

        stdout.write_all(response.to_string().as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}

pub fn json_rpc_response(id: Option<Value>, result: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn json_rpc_error(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
