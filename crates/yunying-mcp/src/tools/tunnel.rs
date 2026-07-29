use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::ToolContext;
use yunying_core::types::AuditAction;

pub(crate) async fn tunnel_create(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let local_port = args["local_port"]
        .as_u64()
        .context("missing 'local_port'")? as u16;
    let remote_host = args["remote_host"]
        .as_str()
        .context("missing 'remote_host'")?
        .to_string();
    let remote_port = args["remote_port"]
        .as_u64()
        .context("missing 'remote_port'")? as u16;
    let local_addr = args["local_addr"].as_str().unwrap_or("127.0.0.1");

    let host = ctx
        .router
        .get(host_name)
        .with_context(|| format!("host not found: {}", host_name))?;

    let result = ctx
        .tunnel_manager
        .create(
            &host,
            local_addr,
            local_port,
            remote_host.clone(),
            remote_port,
            &ctx.ca_cert_path,
            &ctx.bridge_registry,
        )
        .await;

    match result {
        Ok(info) => {
            let detail = format!(
                "{} {}:{} -> {}:{}",
                info.tunnel_id, local_addr, local_port, remote_host, remote_port
            );
            super::audit(
                ctx,
                AuditAction::TunnelCreate,
                host_name,
                "",
                None,
                &detail,
                None,
                true,
                0,
                None,
            )
            .await;
            Ok(json!({
                "ok": true,
                "tunnel_id": info.tunnel_id,
                "local_addr": info.local_addr,
                "remote": format!("{}:{}", info.remote_host, info.remote_port),
            }))
        }
        Err(e) => {
            let detail = format!(
                "{}:{} -> {}:{}",
                local_addr, local_port, remote_host, remote_port
            );
            super::audit(
                ctx,
                AuditAction::TunnelCreate,
                host_name,
                "",
                None,
                &detail,
                None,
                false,
                0,
                Some(&e.to_string()),
            )
            .await;
            Ok(json!({ "ok": false, "error": e.to_string() }))
        }
    }
}

pub(crate) async fn tunnel_list(ctx: &ToolContext) -> Result<Value> {
    let tunnels = ctx.tunnel_manager.list().await;
    super::audit(
        ctx,
        AuditAction::TunnelList,
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
    Ok(json!({
        "ok": true,
        "tunnels": tunnels,
        "count": tunnels.len(),
    }))
}

pub(crate) async fn tunnel_close(ctx: &ToolContext, args: Value) -> Result<Value> {
    let tunnel_id = args["tunnel_id"].as_str().context("missing 'tunnel_id'")?;

    let result = ctx.tunnel_manager.close(tunnel_id).await;
    super::audit(
        ctx,
        AuditAction::TunnelClose,
        "",
        "",
        None,
        tunnel_id,
        None,
        result.is_ok(),
        0,
        None,
    )
    .await;

    match result {
        Ok(()) => Ok(json!({ "ok": true, "closed": tunnel_id })),
        Err(e) => Ok(json!({ "ok": false, "error": e.to_string() })),
    }
}
