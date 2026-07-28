use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::exec::unescape_keys;
use super::ToolContext;
use crate::transport::{connect_to_bridge_hybrid, recv_json_frame, send_json_frame};
use yunying_core::types::AuditAction;

pub(crate) async fn send_keys(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let raw_keys = args["keys"].as_str().context("missing 'keys'")?;
    let keys = unescape_keys(raw_keys);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;

    send_json_frame(&mut tls, &json!({ "type": "send_keys", "session_name": session_name, "pane_id": pane_id, "keys": keys })).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::SendKeys,
        host_name,
        session_name,
        Some(&pane_id),
        &keys,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn capture_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let max_lines = args["max_lines"]
        .as_u64()
        .map(|v| v as usize)
        .unwrap_or(200);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;

    let mut request = json!({
        "type": "capture_pane",
        "session_name": session_name,
        "pane_id": pane_id,
        "max_lines": max_lines,
    });
    if let Some(v) = args.get("ansi") {
        request["ansi"] = v.clone();
    }
    if let Some(v) = args.get("start_line") {
        request["start_line"] = v.clone();
    }
    if let Some(v) = args.get("end_line") {
        request["end_line"] = v.clone();
    }
    if let Some(v) = args.get("join_wrapped") {
        request["join_wrapped"] = v.clone();
    }
    if let Some(v) = args.get("preserve_spaces") {
        request["preserve_spaces"] = v.clone();
    }
    if let Some(v) = args.get("alternate") {
        request["alternate"] = v.clone();
    }
    if let Some(v) = args.get("buffer_name") {
        request["buffer_name"] = v.clone();
    }

    send_json_frame(&mut tls, &request).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    let num_lines = response["text"]
        .as_str()
        .map(|s| s.lines().count())
        .unwrap_or(0);
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::CapturePane,
        host_name,
        session_name,
        Some(&pane_id),
        &format!("{} lines", num_lines),
        None,
        true,
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn resize_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let cols = args["cols"].as_u64().unwrap_or(80);
    let rows = args["rows"].as_u64().unwrap_or(24);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;
    send_json_frame(&mut tls, &json!({ "type": "resize_pane", "session_name": session_name, "pane_id": pane_id, "cols": cols, "rows": rows })).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::ResizePane,
        host_name,
        session_name,
        Some(&pane_id),
        &format!("{}x{}", cols, rows),
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn send_text(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let text = args["text"].as_str().context("missing 'text'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;
    send_json_frame(&mut tls, &json!({ "type": "send_text", "session_name": session_name, "pane_id": pane_id, "text": text })).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::SendText,
        host_name,
        session_name,
        Some(&pane_id),
        text,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn set_pane_title(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let title = args["title"].as_str().context("missing 'title'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;
    send_json_frame(&mut tls, &json!({ "type": "set_pane_title", "session_name": session_name, "pane_id": pane_id, "title": title })).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::SetPaneTitle,
        host_name,
        session_name,
        Some(&pane_id),
        title,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn find_pane_text(ctx: &ToolContext, args: Value) -> Result<Value> {
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
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;
    send_json_frame(&mut tls, &json!({ "type": "find_pane_text", "session_name": session_name, "pane_id": pane_id, "pattern": pattern })).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::FindPaneText,
        host_name,
        session_name,
        Some(&pane_id),
        pattern,
        None,
        response["found"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn split_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let direction = args["direction"].as_str().unwrap_or("horizontal");
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;
    send_json_frame(&mut tls, &json!({ "type": "split_pane", "session_name": session_name, "pane_id": pane_id, "direction": direction })).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::SplitWindow,
        host_name,
        session_name,
        Some(&pane_id),
        direction,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn close_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
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
    send_json_frame(
        &mut tls,
        &json!({ "type": "close_pane", "session_name": session_name, "pane_id": pane_id }),
    )
    .await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::ClosePane,
        host_name,
        session_name,
        Some(pane_id),
        pane_id,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn clear_history(ctx: &ToolContext, args: Value) -> Result<Value> {
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
        &json!({ "type": "clear_history", "session_name": session_name, "pane_id": pane_id }),
    )
    .await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::ClearHistory,
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

pub(crate) async fn split_pane_with(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let direction = args["direction"].as_str().context("missing 'direction'")?;
    let command = args["command"].as_str().context("missing 'command'")?;
    let cmd_args = args["args"].as_array().cloned().unwrap_or_default();
    let shell = args["shell"].as_bool().unwrap_or(true);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;

    let mut request = json!({
        "type": "split_pane_with",
        "session_name": session_name,
        "pane_id": pane_id,
        "direction": direction,
        "command": command,
        "args": cmd_args,
        "shell": shell,
    });
    if let Some(v) = args.get("cwd") {
        request["cwd"] = v.clone();
    }
    if let Some(v) = args.get("env") {
        request["env"] = v.clone();
    }
    if let Some(v) = args.get("title") {
        request["title"] = v.clone();
    }
    if let Some(v) = args.get("keep_alive_on_exit") {
        request["keep_alive_on_exit"] = v.clone();
    }

    send_json_frame(&mut tls, &request).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::SplitPaneWith,
        host_name,
        session_name,
        Some(&pane_id),
        command,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn get_pane_title(ctx: &ToolContext, args: Value) -> Result<Value> {
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
        &json!({
            "type": "get_pane_title",
            "session_name": session_name,
            "pane_id": pane_id,
        }),
    )
    .await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::GetPaneTitle,
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

pub(crate) async fn get_pane_by_title(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let title = args["title"].as_str().context("missing 'title'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(
        &mut tls,
        &json!({ "type": "get_pane_by_title", "title": title }),
    )
    .await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::GetPaneByTitle,
        host_name,
        "",
        None,
        title,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn break_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id = args["pane_id"].as_str().unwrap_or("");
    let destination_window = args["destination_window"].as_u64();
    let detached = args["detached"].as_bool().unwrap_or(false);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let mut req = json!({
        "type": "break_pane",
        "session_name": session_name,
        "pane_id": pane_id,
        "detached": detached,
    });
    if let Some(dw) = destination_window {
        req["destination_window"] = json!(dw);
    }
    send_json_frame(&mut tls, &req).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::BreakPane,
        host_name,
        session_name,
        None,
        pane_id,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn join_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let source_pane_id = args["source_pane_id"]
        .as_str()
        .context("missing 'source_pane_id'")?;
    let target_pane_id = args["target_pane_id"]
        .as_str()
        .context("missing 'target_pane_id'")?;
    let direction = args["direction"].as_str();
    let size = args["size"].as_u64();
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let mut req = json!({
        "type": "join_pane",
        "session_name": session_name,
        "source_pane_id": source_pane_id,
        "target_pane_id": target_pane_id,
    });
    if let Some(d) = direction {
        req["direction"] = json!(d);
    }
    if let Some(s) = size {
        req["size"] = json!(s);
    }
    send_json_frame(&mut tls, &req).await?;
    let response = recv_json_frame(&mut tls).await?;
    let detail = format!("{} -> {}", source_pane_id, target_pane_id);
    super::audit(
        ctx,
        AuditAction::JoinPane,
        host_name,
        session_name,
        None,
        &detail,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn swap_pane(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let source_pane_id = args["source_pane_id"]
        .as_str()
        .context("missing 'source_pane_id'")?;
    let target_pane_id = args["target_pane_id"]
        .as_str()
        .context("missing 'target_pane_id'")?;
    let detached = args["detached"].as_bool().unwrap_or(false);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    send_json_frame(
        &mut tls,
        &json!({
            "type": "swap_pane",
            "session_name": session_name,
            "source_pane_id": source_pane_id,
            "target_pane_id": target_pane_id,
            "detached": detached,
        }),
    )
    .await?;
    let response = recv_json_frame(&mut tls).await?;
    let detail = format!("{} <-> {}", source_pane_id, target_pane_id);
    super::audit(
        ctx,
        AuditAction::SwapPane,
        host_name,
        session_name,
        None,
        &detail,
        None,
        response["ok"].as_bool().unwrap_or(false),
        0,
        None,
    )
    .await;
    Ok(response)
}

pub(crate) async fn capture_region(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"]
        .as_str()
        .context("missing 'session_name'")?;
    let pane_id_arg = args["pane_id"].as_str();
    let styled = args["styled"].as_bool().unwrap_or(false);
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls =
        connect_to_bridge_hybrid(&host.bridge_addr, &host.bridge_token, &ctx.ca_cert_path, 3)
            .await?;
    let (pane_id, auto_resolved) =
        super::common::resolve_pane_id(&mut tls, session_name, pane_id_arg).await?;

    let mut request = json!({
        "type": "capture_region",
        "session_name": session_name,
        "pane_id": pane_id,
        "styled": styled,
    });
    if let Some(v) = args.get("row") {
        request["row"] = v.clone();
    }
    if let Some(v) = args.get("col") {
        request["col"] = v.clone();
    }
    if let Some(v) = args.get("rows") {
        request["rows"] = v.clone();
    }
    if let Some(v) = args.get("cols") {
        request["cols"] = v.clone();
    }

    send_json_frame(&mut tls, &request).await?;
    let mut response = recv_json_frame(&mut tls).await?;
    super::common::enrich_pane_response(&mut response, &pane_id, auto_resolved);
    super::audit(
        ctx,
        AuditAction::CaptureRegion,
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
