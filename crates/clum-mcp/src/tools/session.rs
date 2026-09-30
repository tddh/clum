use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::ToolContext;
use crate::transport::{connect_to_host, recv_json_frame, send_json_frame};
use clum_core::types::AuditAction;

pub(crate) async fn reload_config(ctx: &ToolContext) -> Result<Value> {
    let kb_status = reload_command_kb(ctx);
    let rules_status = reload_rules(ctx);

    match ctx.router.reload() {
        Ok(count) => {
            super::audit(
                ctx,
                AuditAction::HostList,
                "",
                "",
                None,
                &format!(
                    "reloaded {} hosts; command kb: {}; command rules: {}",
                    count, kb_status, rules_status
                ),
                None,
                true,
                0,
                None,
            )
            .await;
            Ok(json!({
                "ok": true,
                "hosts_count": count,
                "command_kb": kb_status,
                "command_rules": rules_status,
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
                "command_kb": kb_status,
                "command_rules": rules_status,
            }))
        }
    }
}

/// 重载命令知识库（内嵌默认值 + 运行时覆盖文件）。解析失败保留旧知识库。
fn reload_command_kb(ctx: &ToolContext) -> String {
    match crate::load_command_kb(&ctx.command_kb_path) {
        Ok(new_kb) => {
            let count = new_kb.len();
            *ctx.command_kb.write().unwrap_or_else(|e| e.into_inner()) = new_kb;
            tracing::info!(
                "reloaded {count} command knowledge base entries from {}",
                ctx.command_kb_path.display()
            );
            format!("reloaded {count}")
        }
        Err(e) => {
            let kept = ctx
                .command_kb
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len();
            tracing::error!("command kb reload failed ({e}) — keeping previous {kept} entries");
            format!("unchanged ({kept} kept): {e}")
        }
    }
}

/// 重载命令规则。解析失败时保留旧规则（绝不因配置错误退化为「无规则」），
/// 并把结果作为字符串返回，让调用方知道规则是否真的生效。
fn reload_rules(ctx: &ToolContext) -> String {
    match std::fs::read_to_string(&ctx.rules_path)
        .map_err(|e| e.to_string())
        .and_then(|text| clum_command_kb::RuleSet::from_toml_str(&text).map_err(|e| e.to_string()))
    {
        Ok(new_rules) => {
            let count = new_rules.rule.len();
            *ctx.rules.write().unwrap_or_else(|e| e.into_inner()) = new_rules;
            tracing::info!(
                "reloaded {count} command rules from {}",
                ctx.rules_path.display()
            );
            format!("reloaded {count}")
        }
        Err(e) => {
            let kept = ctx
                .rules
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .rule
                .len();
            tracing::error!("command rules reload failed ({e}) — keeping previous {kept} rules");
            format!("unchanged ({kept} rules kept): {e}")
        }
    }
}

pub(crate) async fn session_create(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().unwrap_or("clum");
    let host = super::common::resolve_host_config(ctx, host_name).await?;

    let mut tls = connect_to_host(ctx, &host).await?;

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
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let mut tls = connect_to_host(ctx, &host).await?;

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
    let session_name = args["session_name"].as_str().unwrap_or("clum");
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let mut tls = connect_to_host(ctx, &host).await?;

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

pub(crate) async fn kill_session(ctx: &ToolContext, args: Value) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let session_name = args["session_name"].as_str().unwrap_or("clum");
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let mut tls = connect_to_host(ctx, &host).await?;
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
    let session_name = args["session_name"].as_str().unwrap_or("clum");
    let pane_id = args["pane_id"].as_str().context("missing 'pane_id'")?;
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let mut tls = connect_to_host(ctx, &host).await?;

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
