use crate::progress::ProgressReporter;
use crate::tools;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

pub async fn run_mcp_stdio_loop(
    ctx: Arc<tools::ToolContext>,
    tools_def: Value,
) -> anyhow::Result<()> {
    let stdin = BufReader::new(stdin());
    let stdout = Arc::new(Mutex::new(stdout()));
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = json_rpc_error(None, -32700, &format!("Parse error: {e}"));
                let mut w = stdout.lock().await;
                w.write_all(err.to_string().as_bytes()).await?;
                w.write_all(b"\n").await?;
                w.flush().await?;
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
                let meta_token = &request["params"]["_meta"]["progressToken"];
                let progress_token = if meta_token.is_null() {
                    None
                } else {
                    Some(meta_token.clone())
                };
                let mut reporter =
                    ProgressReporter::new_stdout(progress_token, Arc::clone(&stdout));
                match tools::execute_tool(&ctx, tool_name, args, &mut reporter).await {
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
                        "capabilities": { "tools": {}, "progress": {} },
                        "serverInfo": { "name": "clum-mcp", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": crate::schema::instructions()
                    }),
                )
            }
            _ => json_rpc_error(id, -32601, &format!("Method not found: {method}")),
        };

        let mut w = stdout.lock().await;
        w.write_all(response.to_string().as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
    }

    Ok(())
}

pub fn json_rpc_response(id: Option<Value>, result: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn json_rpc_error(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
