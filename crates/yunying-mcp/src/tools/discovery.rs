use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::ToolContext;
use crate::transport::{connect_to_host, recv_json_frame, send_json_frame};
use yunying_core::types::AuditAction;

struct UnifiedHost {
    name: String,
    group: String,
    tags: Vec<String>,
    labels: HashMap<String, String>,
    bridge_addr: String,
    online: bool,
    via: &'static str,
}

async fn build_unified_hosts(ctx: &ToolContext) -> Vec<UnifiedHost> {
    let meta_list = ctx.bridge_store.get_all_host_meta().await;
    let meta_map: HashMap<String, crate::bridge_store::HostMeta> = meta_list
        .into_iter()
        .map(|m| (m.hostname.clone(), m))
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut hosts = Vec::new();

    for info in ctx.bridge_registry.list().await {
        seen.insert(info.hostname.clone());
        let meta = meta_map.get(&info.hostname);
        hosts.push(UnifiedHost {
            name: info.hostname.clone(),
            group: meta.map(|m| m.group.clone()).unwrap_or_default(),
            tags: meta
                .map(|m| m.tags.clone())
                .unwrap_or_else(|| info.tags.clone()),
            labels: meta
                .map(|m| m.labels.clone())
                .unwrap_or_else(|| info.labels.clone()),
            bridge_addr: String::new(),
            online: true,
            via: "enrolled",
        });
    }

    for h in ctx.router.list() {
        if seen.contains(&h.name) {
            continue;
        }
        hosts.push(UnifiedHost {
            name: h.name,
            group: h.group,
            tags: h.tags,
            labels: h.labels,
            bridge_addr: h.bridge_addr,
            online: false,
            via: "direct",
        });
    }

    hosts
}

fn host_to_json(h: &UnifiedHost) -> Value {
    json!({
        "name": h.name,
        "group": h.group,
        "tags": h.tags,
        "labels": h.labels,
        "bridge_addr": h.bridge_addr,
        "online": if h.online { json!(true) } else { Value::Null },
        "via": h.via,
    })
}

pub(crate) async fn host_list(ctx: &ToolContext) -> Result<Value> {
    let unified = build_unified_hosts(ctx).await;
    let hosts: Vec<Value> = unified.iter().map(host_to_json).collect();
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
    let mut hosts = build_unified_hosts(ctx).await;

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

    let result: Vec<Value> = hosts.iter().map(host_to_json).collect();
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

pub(crate) async fn host_set_meta(ctx: &ToolContext, args: Value) -> Result<Value> {
    let hostname = args["host"].as_str().context("missing 'host'")?;
    let group = args.get("group").and_then(|v| v.as_str());
    let tags: Option<Vec<String>> = args.get("tags").and_then(|v| v.as_array()).map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    let labels: Option<HashMap<String, String>> =
        args.get("labels").and_then(|v| v.as_object()).map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        });

    if group.is_none() && tags.is_none() && labels.is_none() {
        return Ok(
            json!({"ok": false, "error": "nothing to update: provide group, tags, or labels"}),
        );
    }

    let found = ctx
        .bridge_store
        .set_host_meta(hostname, group, tags.as_deref(), labels.as_ref())
        .await?;

    if !found {
        return Ok(
            json!({"ok": false, "error": format!("host '{}' not found in enrolled bridges", hostname)}),
        );
    }

    Ok(
        json!({"ok": true, "host": hostname, "updated": {"group": group, "tags": tags, "labels": labels}}),
    )
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
