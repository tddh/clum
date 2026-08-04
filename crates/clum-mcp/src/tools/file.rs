use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::ToolContext;
use clum_core::types::AuditAction;

pub(crate) async fn file_upload(
    ctx: &ToolContext,
    args: Value,
    progress: &mut crate::progress::ProgressReporter,
) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let local_path = args["local_path"]
        .as_str()
        .context("missing 'local_path'")?;
    let remote_path = args["remote_path"]
        .as_str()
        .context("missing 'remote_path'")?;
    let host = super::common::resolve_host_config(ctx, host_name).await?;
    let overwrite = match args["overwrite"].as_str().unwrap_or("overwrite") {
        "skip" => crate::files::OverwriteMode::Skip,
        "rename" => crate::files::OverwriteMode::Rename,
        "error" => crate::files::OverwriteMode::NoClobber,
        _ => crate::files::OverwriteMode::Overwrite,
    };
    let exclude: Vec<String> = args["exclude"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let result = crate::files::upload_file(
        &host,
        local_path,
        remote_path,
        &ctx.ca_cert_path,
        overwrite,
        &exclude,
        progress,
        &ctx.bridge_registry,
    )
    .await;
    super::audit(
        ctx,
        AuditAction::FileUpload,
        host_name,
        "",
        None,
        local_path,
        None,
        result.is_ok(),
        0,
        None,
    )
    .await;

    match result {
        Ok(files) => Ok(json!({
            "ok": true,
            "files": files,
            "total": files.len(),
            "uploaded": files.iter().filter(|f| f.status == "uploaded").count(),
            "skipped": files.iter().filter(|f| f.status == "skipped").count(),
            "failed": 0,
        })),
        Err(e) => Ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

pub(crate) async fn file_download(
    ctx: &ToolContext,
    args: Value,
    progress: &mut crate::progress::ProgressReporter,
) -> Result<Value> {
    let host_name = args["host"].as_str().context("missing 'host'")?;
    let remote_path = args["remote_path"]
        .as_str()
        .context("missing 'remote_path'")?;
    let local_path = args["local_path"]
        .as_str()
        .context("missing 'local_path'")?;
    let host = super::common::resolve_host_config(ctx, host_name).await?;

    let result = crate::files::download_file(
        &host,
        remote_path,
        local_path,
        &ctx.ca_cert_path,
        progress,
        &ctx.bridge_registry,
    )
    .await;
    super::audit(
        ctx,
        AuditAction::FileDownload,
        host_name,
        "",
        None,
        remote_path,
        None,
        result.is_ok(),
        0,
        None,
    )
    .await;

    match result {
        Ok(files) => {
            if files.len() == 1 {
                Ok(json!({
                    "ok": true,
                    "file": {
                        "uri": format!("file://{}/{}", host_name, remote_path),
                        "local_path": files[0].path,
                        "size": files[0].size,
                        "sha256": files[0].sha256,
                    }
                }))
            } else {
                Ok(json!({
                    "ok": true,
                    "files": files,
                    "total": files.len(),
                }))
            }
        }
        Err(e) => Ok(json!({ "ok": false, "error": e.to_string() })),
    }
}
