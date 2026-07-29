use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::ToolContext;
use crate::transport::{connect_to_host, recv_json_frame, send_json_frame};
use yunying_core::types::AuditAction;

pub(crate) async fn wait_exit(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_host(ctx, &host).await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;
    send_json_frame(&mut tls, &json!({ "type": "wait_exit", "session_name": session_name, "pane_id": pane_id, "timeout_ms": timeout_ms })).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::WaitExit,
        host_name,
        session_name,
        Some(&pane_id),
        "",
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn wait_for_text(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let text = args["text"].as_str().context("missing 'text'")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_host(ctx, &host).await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;

    send_json_frame(&mut tls, &json!({ "type": "wait_for_text", "session_name": session_name, "pane_id": pane_id, "text": text, "timeout_ms": timeout_ms })).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::WaitForText,
        host_name,
        session_name,
        Some(&pane_id),
        text,
        None,
        response["found"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn find_text_all(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let pattern = args["pattern"].as_str().context("missing 'pattern'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_host(ctx, &host).await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;

    send_json_frame(
        &mut tls,
        &json!({
            "type": "find_text_all",
            "session_name": session_name,
            "pane_id": pane_id,
            "pattern": pattern,
        }),
    )
    .await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::FindTextAll,
        host_name,
        session_name,
        Some(&pane_id),
        pattern,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn wait_for_bytes(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let bytes_b64 = args["bytes"].as_str().context("missing 'bytes'")?;
    let only_new = args["only_new"].as_bool().unwrap_or(false);
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_host(ctx, &host).await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;

    send_json_frame(
        &mut tls,
        &json!({
            "type": "wait_for_bytes",
            "session_name": session_name,
            "pane_id": pane_id,
            "bytes": bytes_b64,
            "only_new": only_new,
            "timeout_ms": timeout_ms,
        }),
    )
    .await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::WaitForBytes,
        host_name,
        session_name,
        Some(&pane_id),
        bytes_b64,
        None,
        response["found"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn wait_stable(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let stable_ms = args["stable_ms"].as_u64().unwrap_or(500);
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_host(ctx, &host).await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;

    send_json_frame(
        &mut tls,
        &json!({
            "type": "wait_stable",
            "session_name": session_name,
            "pane_id": pane_id,
            "stable_ms": stable_ms,
            "timeout_ms": timeout_ms,
        }),
    )
    .await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::WaitStable,
        host_name,
        session_name,
        Some(&pane_id),
        "",
        None,
        response["stable"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}
