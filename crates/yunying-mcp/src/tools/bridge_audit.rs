use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::ToolContext;
use crate::transport::{connect_to_bridge_hybrid_stream, recv_json_frame, send_json_frame};

/// Query the bridge-side connection event log on a remote host.
///
/// Opens a QUIC JSON protocol stream (0x01) to the target bridge and sends an
/// `audit_query` command, which the bridge answers from its own BridgeAuditDb
/// (auth events, attach/detach, file ops, tunnels, ...).
pub(crate) async fn query_bridge_audit(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;

    let event_type = args["event_type"].as_str();
    let session_name = args["session_name"].as_str();
    let since = args["since"].as_str();
    let until = args["until"].as_str();
    let limit = args["limit"].as_u64().unwrap_or(50);

    let mut params = json!({ "limit": limit });
    if let Some(v) = event_type {
        params["event_type"] = json!(v);
    }
    if let Some(v) = session_name {
        params["session_name"] = json!(v);
    }
    if let Some(v) = since {
        params["since"] = json!(v);
    }
    if let Some(v) = until {
        params["until"] = json!(v);
    }

    let command = json!({
        "command": "audit_query",
        "params": params,
    });

    let mut stream = connect_to_bridge_hybrid_stream(
        &host.bridge_addr,
        &host.bridge_token,
        &ctx.ca_cert_path,
        3,
        30,
        10,
    )
    .await
    .with_context(|| format!("failed to connect to bridge: {}", host_name))?;

    send_json_frame(&mut stream, &command)
        .await
        .context("failed to send audit_query command")?;
    let response = recv_json_frame(&mut stream)
        .await
        .context("failed to receive audit_query response")?;

    Ok(response)
}
