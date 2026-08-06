use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::ToolContext;
use crate::transport::{connect_to_host, recv_json_frame, send_json_frame};
use clum_core::types::AuditAction;

pub(crate) async fn list_buffers(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let mut tls = connect_to_host(ctx, &host).await?;
    send_json_frame(&mut tls, &json!({ "type": "list_buffers" })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::ListBuffers,
        host_name,
        "",
        None,
        "",
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn paste_buffer(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().unwrap_or("clum");
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let buffer_name = args["buffer_name"].as_str().unwrap_or("");
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let mut tls = connect_to_host(ctx, &host).await?;
    send_json_frame(&mut tls, &json!({ "type": "paste_buffer", "session_name": session_name, "pane_id": pane_id, "buffer_name": buffer_name })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::PasteBuffer,
        host_name,
        session_name,
        Some(pane_id),
        buffer_name,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn delete_buffer(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let buffer_name = args["buffer_name"]
        .as_str()
        .context("missing 'buffer_name'")?;
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let mut tls = connect_to_host(ctx, &host).await?;
    send_json_frame(
        &mut tls,
        &json!({ "type": "delete_buffer", "buffer_name": buffer_name }),
    )
    .await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::DeleteBuffer,
        host_name,
        "",
        None,
        buffer_name,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}
