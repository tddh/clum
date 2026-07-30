use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::ToolContext;
use crate::transport::{connect_to_host, recv_json_frame, send_json_frame};
use yunying_core::types::AuditAction;

pub(crate) async fn host_list(ctx: &ToolContext) -> Result<Value> {
    let mut hosts: Vec<Value> = Vec::new();
    for h in ctx.router.list() {
        let hub_online = ctx.bridge_registry.is_online(&h.name).await;
        let online_val = if hub_online { json!(true) } else { Value::Null };
        hosts.push(json!({
            "name": h.name,
            "group": h.group,
            "tags": h.tags,
            "labels": h.labels,
            "bridge_addr": h.bridge_addr,
            "online": online_val,
            "via": if hub_online { "enrolled" } else { "direct" },
        }));
    }
    super::audit(
        ctx,
        AuditAction::HostList,
        "",
        "",
        None,
        "",
        None,
        true,
        0,
        None,
    )
    .await;
    Ok(json!({ "hosts": hosts, "count": hosts.len() }))
}

pub(crate) async fn host_filter(ctx: &ToolContext, args: Value) -> Result<Value> {
    let mut hosts: Vec<yunying_core::types::HostConfig> = ctx.router.list();

    if let Some(group) = args["group"].as_str() {
        hosts.retain(|h| h.group == group);
    }
    if let Some(tags) = args["tags"].as_array() {
        let tags: Vec<&str> = tags.iter().filter_map(|v| v.as_str()).collect();
        hosts.retain(|h| tags.iter().all(|t| h.tags.contains(&t.to_string())));
    }
    if let Some(key) = args["label_key"].as_str() {
        if let Some(value) = args["label_value"].as_str() {
            hosts.retain(|h| h.labels.get(key).map(|v| v == value).unwrap_or(false));
        }
    }
    if let Some(pattern) = args["pattern"].as_str() {
        if let Ok(pat) = glob::Pattern::new(pattern) {
            hosts.retain(|h| pat.matches(&h.name));
        }
    }

    let mut result: Vec<Value> = Vec::new();
    for h in &hosts {
        let hub_online = ctx.bridge_registry.is_online(&h.name).await;
        let online_val = if hub_online { json!(true) } else { Value::Null };
        result.push(json!({
            "name": h.name, "group": h.group, "tags": h.tags, "labels": h.labels,
            "bridge_addr": h.bridge_addr,
            "online": online_val,
            "via": if hub_online { "enrolled" } else { "direct" },
        }));
    }
    super::audit(
        ctx,
        AuditAction::HostFilter,
        "",
        "",
        None,
        "",
        Some(&format!(
            "group={:?} tags={:?} pattern={:?} label_key={:?}",
            args.get("group"),
            args.get("tags"),
            args.get("pattern"),
            args.get("label_key")
        )),
        true,
        0,
        None,
    )
    .await;
    Ok(json!({ "hosts": result, "count": result.len() }))
}

pub(crate) async fn find_panes(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_host(ctx, &host).await?;

    let mut request = json!({"type": "find_panes"});
    if let Some(v) = args.get("session_name") {
        request["session_name"] = v.clone();
    }
    if let Some(v) = args.get("title") {
        request["title"] = v.clone();
    }
    if let Some(v) = args.get("title_prefix") {
        request["title_prefix"] = v.clone();
    }
    if let Some(v) = args.get("command_contains") {
        request["command_contains"] = v.clone();
    }
    if let Some(v) = args.get("cwd_contains") {
        request["cwd_contains"] = v.clone();
    }
    if let Some(v) = args.get("window_index") {
        request["window_index"] = v.clone();
    }
    if let Some(v) = args.get("running") {
        request["running"] = v.clone();
    }
    if let Some(v) = args.get("exited") {
        request["exited"] = v.clone();
    }

    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::FindPanes,
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

pub(crate) async fn find_sessions(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_host(ctx, &host).await?;

    let mut request = json!({"type": "find_sessions"});
    if let Some(v) = args.get("name") {
        request["name"] = v.clone();
    }

    send_json_frame(&mut tls, &request).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::FindSessions,
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

pub(crate) async fn host_capabilities(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let check = args["check"].as_str();
    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;
    let mut tls = connect_to_host(ctx, &host).await?;
    let mut req = json!({ "type": "capabilities" });
    if let Some(c) = check {
        req["check"] = json!(c);
    }
    send_json_frame(&mut tls, &req).await?;
    let response = recv_json_frame(&mut tls).await?;
    super::audit(
        ctx,
        AuditAction::HostCapabilities,
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
