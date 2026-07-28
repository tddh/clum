use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::ToolContext;
use crate::transport::{connect_to_bridge_hybrid, recv_json_frame, send_json_frame};
use yunying_core::types::AuditAction;

pub(crate) async fn split_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let direction = args["direction"].as_str().unwrap_or("horizontal");
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;

    send_json_frame(
        &mut tls,
        &json!({ "type": "split_window", "session_name": session_name, "direction": direction }),
    )
    .await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::SplitWindow,
        host_name,
        session_name,
        None,
        session_name,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn stream_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10000);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;

    let (pane_id, auto_resolved) = match pane_id_arg {
        Some(id) => (id.to_string(), false),
        None => {
            let mut tls = connect_to_bridge_hybrid(
                &host.bridge_addr,
                &host.bridge_token,
                &ctx.ca_cert_path,
                3,
            )
            .await?;
            super::common::resolve_pane_id(&mut tls, session_name, None).await?
        }
    };

    let start = std::time::Instant::now();
    let mut response = ctx
        .stream_manager
        .stream_pane(&host, session_name, &pane_id, timeout_ms, &ctx.ca_cert_path)
        .await?;
    let elapsed = start.elapsed().as_millis() as u64;

    let has_data = response["text"]
        .as_str()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::StreamSubscribe,
        host_name,
        session_name,
        Some(&pane_id),
        "",
        None,
        has_data,
        elapsed,
        None,
    )
    .await;

    Ok(response)
}

pub(crate) async fn close_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let window_index = args["window_index"]
        .as_u64()
        .context("missing 'window_index'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(&mut tls, &json!({ "type": "close_window", "session_name": session_name, "window_index": window_index })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::CloseWindow,
        host_name,
        session_name,
        None,
        &format!("win_{}", window_index),
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn rename_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let window_index = args["window_index"]
        .as_u64()
        .context("missing 'window_index'")?;
    let name = args["name"].as_str().context("missing 'name'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(&mut tls, &json!({ "type": "rename_window", "session_name": session_name, "window_index": window_index, "name": name })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::RenameWindow,
        host_name,
        session_name,
        None,
        &format!("win_{}", window_index),
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn list_window_panes(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let window_index = args["window_index"]
        .as_u64()
        .context("missing 'window_index'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(&mut tls, &json!({ "type": "list_window_panes", "session_name": session_name, "window_index": window_index })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::ListWindowPanes,
        host_name,
        session_name,
        None,
        &format!("win_{}", window_index),
        None,
        response["ok"].as_bool().unwrap_or(true),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn resize_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let window_index = args["window_index"]
        .as_u64()
        .context("missing 'window_index'")?;
    let width = args["width"]
        .as_u64()
        .map(|v| u16::try_from(v).context("width must be 0-65535"))
        .transpose()?;
    let height = args["height"]
        .as_u64()
        .map(|v| u16::try_from(v).context("height must be 0-65535"))
        .transpose()?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(&mut tls, &json!({ "type": "resize_window", "session_name": session_name, "window_index": window_index, "width": width, "height": height })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::ResizeWindow,
        host_name,
        session_name,
        None,
        &format!("win_{}", window_index),
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn select_window(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let window_index = args["window_index"]
        .as_u64()
        .context("missing 'window_index'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(&mut tls, &json!({ "type": "select_window", "session_name": session_name, "window_index": window_index })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::SelectWindow,
        host_name,
        session_name,
        None,
        &format!("win_{}", window_index),
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn select_layout(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let window_index = args["window_index"]
        .as_u64()
        .context("missing 'window_index'")?;
    let layout = args["layout"].as_str().context("missing 'layout'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(&mut tls, &json!({ "type": "select_layout", "session_name": session_name, "window_index": window_index, "layout": layout })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::SelectLayout,
        host_name,
        session_name,
        None,
        &format!("win_{}", window_index),
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn pane_info(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;
    send_json_frame(
        &mut tls,
        &json!({ "type": "pane_info", "session_name": session_name, "pane_id": pane_id }),
    )
    .await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::PaneInfo,
        host_name,
        session_name,
        Some(&pane_id),
        "",
        None,
        response["ok"].as_bool().unwrap_or(true),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn window_info(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let window_index = args["window_index"]
        .as_u64()
        .context("missing 'window_index'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(&mut tls, &json!({ "type": "window_info", "session_name": session_name, "window_index": window_index })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::WindowInfo,
        host_name,
        session_name,
        None,
        &format!("win_{}", window_index),
        None,
        response["ok"].as_bool().unwrap_or(true),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn pane_exists(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;
    send_json_frame(
        &mut tls,
        &json!({ "type": "pane_exists", "session_name": session_name, "pane_id": pane_id }),
    )
    .await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::PaneExists,
        host_name,
        session_name,
        Some(&pane_id),
        &pane_id,
        None,
        response["exists"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}
