use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::ToolContext;
use crate::transport::{connect_to_bridge_hybrid, recv_json_frame, send_json_frame};
use yunying_core::types::AuditAction;

pub(crate) async fn reload_config(ctx: &ToolContext) -> Result<Value> {
    match ctx.router.reload() {
        Ok(count) => {
            super::audit(
                ctx,
                AuditAction::HostList,
                "",
                "",
                None,
                &format!("reloaded {} hosts", count),
                None,
                true,
                0,
                None,
            )
            .await;
            Ok(json!({
                "ok": true,
                "hosts_count": count,
                "message": format!("successfully reloaded {} hosts", count),
            }))
        }
        Err(e) => {
            let err_msg = e.to_string();
            super::audit(
                ctx,
                AuditAction::HostList,
                "",
                "",
                None,
                "",
                None,
                false,
                0,
                Some(&err_msg),
            )
            .await;
            Ok(json!({
                "ok": false,
                "error": err_msg,
            }))
        }
    }
}

pub(crate) async fn session_create(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().unwrap_or("yunying");
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;

    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;

    let request = json!({ "type": "new_session", "name": session_name, "detached": true });
    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;

    super::audit(
        ctx,
        AuditAction::SessionCreate,
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

pub(crate) async fn session_list(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;

    send_json_frame(&mut tls, &json!({ "type": "list_sessions" })).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::SessionList,
        host_name,
        "",
        None,
        "",
        None,
        response["ok"].as_bool().unwrap_or(true),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn session_attach(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;

    send_json_frame(
        &mut tls,
        &json!({ "type": "attach_session", "session_name": session_name }),
    )
    .await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::SessionAttach,
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

pub(crate) async fn session_detach(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;

    send_json_frame(
        &mut tls,
        &json!({ "type": "detach_session", "session_name": session_name }),
    )
    .await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::SessionDetach,
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

pub(crate) async fn kill_session(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(
        &mut tls,
        &json!({ "type": "kill_session", "session_name": session_name }),
    )
    .await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::KillSession,
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

pub(crate) async fn respawn_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;

    let mut request = json!({
        "type": "respawn_pane",
        "session_name": session_name,
        "pane_id": pane_id,
    });
    if let Some(v) = args.get("command") {
        request["command"] = v.clone();
    }
    if let Some(v) = args.get("args") {
        request["args"] = v.clone();
    }
    if let Some(v) = args.get("shell") {
        request["shell"] = v.clone();
    }
    if let Some(v) = args.get("cwd") {
        request["cwd"] = v.clone();
    }
    if let Some(v) = args.get("env") {
        request["env"] = v.clone();
    }
    if let Some(v) = args.get("kill") {
        request["kill"] = v.clone();
    }
    if let Some(v) = args.get("keep_alive_on_exit") {
        request["keep_alive_on_exit"] = v.clone();
    }

    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::RespawnPane,
        host_name,
        session_name,
        Some(pane_id),
        "",
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}
