use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    Router,
};
use rmcp::{
    model::*,
    service::RequestContext,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    RoleServer, ServerHandler,
};

use crate::api_keys::ApiKeyStore;
use crate::tools::{self, ToolContext};

pub type DownloadTokenMap =
    Arc<tokio::sync::RwLock<std::collections::HashMap<String, std::time::Instant>>>;

#[derive(Clone)]
struct AuthState {
    store: Arc<ApiKeyStore>,
    download_tokens: DownloadTokenMap,
    bridge_store: Arc<crate::bridge_store::BridgeStore>,
}

/// Caller identity derived from the request's own API key. Inserted into
/// request extensions by the auth middleware and read per-request in
/// call_tool — never shared between concurrent requests.
#[derive(Clone)]
struct AuthIdentity {
    name: String,
    group: Option<String>,
}

#[derive(Clone)]
pub struct YunyingServer {
    ctx: Arc<ToolContext>,
    key_store: Arc<ApiKeyStore>,
}

impl ServerHandler for YunyingServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(crate::schema::instructions())
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, rmcp::ErrorData>> + Send + '_
    {
        let tools = crate::schema::tools_as_rmcp();
        std::future::ready(Ok(ListToolsResult {
            tools,
            ..Default::default()
        }))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResponse, rmcp::ErrorData>> + Send + '_
    {
        // Per-request context clone: identity fields are isolated from other
        // concurrent requests; shared resources (Arc) clone cheaply.
        let ctx = self.ctx.as_ref().clone();
        let identity = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<AuthIdentity>().cloned());
        let tool_name = request.name.to_string();
        let args = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let peer = context.peer.clone();
        let progress_token = context.meta.get_progress_token();
        let key_store = Arc::clone(&self.key_store);

        async move {
            match identity {
                Some(id) => {
                    *ctx.agent_name.lock().unwrap_or_else(|e| e.into_inner()) = id.name;
                    *ctx.caller_group.lock().unwrap_or_else(|e| e.into_inner()) = id.group;
                }
                None => {
                    // Fail closed: with auth enabled, every tool call must
                    // carry an identity derived from its own API key. A
                    // missing identity would otherwise run as superadmin.
                    if !key_store.is_empty().await {
                        tracing::error!(
                            "call_tool without caller identity while auth is enabled — rejecting"
                        );
                        return Err(rmcp::ErrorData::internal_error(
                            "missing caller identity",
                            None,
                        ));
                    }
                }
            }
            let mut reporter = crate::progress::ProgressReporter::new_peer(progress_token, peer);
            match tools::execute_tool(&ctx, &tool_name, args, &mut reporter).await {
                Ok(mut result) => {
                    crate::error::enrich_error(&mut result);
                    let is_error = result.get("ok").and_then(|v| v.as_bool()) == Some(false);
                    let text = result.to_string();
                    let mut call_result = CallToolResult::success(vec![ContentBlock::text(text)]);
                    if is_error {
                        call_result.is_error = Some(true);
                    }
                    Ok(CallToolResponse::Complete(call_result))
                }
                Err(e) => {
                    if e.to_string().starts_with("unknown tool") {
                        Err(rmcp::ErrorData::invalid_params(format!("{e:#}"), None))
                    } else {
                        let result = crate::error::error_result(&e);
                        let call_result =
                            CallToolResult::error(vec![ContentBlock::text(result.to_string())]);
                        Ok(CallToolResponse::Complete(call_result))
                    }
                }
            }
        }
    }
}

async fn auth_middleware(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if auth.store.is_empty().await {
        return Ok(next.run(request).await);
    }

    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    let is_mcp = request.uri().path().starts_with("/mcp");

    match token {
        Some(t) => {
            if let Some(identity) = auth.store.validate(t).await {
                // Grouped keys must use MCP get_recording (which enforces
                // group isolation); direct HTTP access to /recordings is
                // restricted to superadmin keys without a group.
                if identity.group.is_some()
                    && request.uri().path().starts_with("/recordings")
                {
                    tracing::warn!(
                        agent = %identity.name,
                        group = %identity.group.as_deref().unwrap_or(""),
                        path = %request.uri().path(),
                        "grouped key attempted HTTP /recordings access (use MCP get_recording instead)"
                    );
                    return Err(StatusCode::FORBIDDEN);
                }
                // Bind the identity to THIS request only — call_tool reads it
                // back from the request extensions, so concurrent requests
                // from different agents can never overwrite each other.
                request.extensions_mut().insert(AuthIdentity {
                    name: identity.name,
                    group: identity.group,
                });
                return Ok(next.run(request).await);
            }
            if !is_mcp {
                if t.starts_with("dl_") {
                    use sha2::{Digest, Sha256};
                    let hash = hex::encode(Sha256::digest(t.as_bytes()));
                    let tokens = auth.download_tokens.read().await;
                    if let Some(expires) = tokens.get(&hash) {
                        if *expires > std::time::Instant::now() {
                            return Ok(next.run(request).await);
                        }
                    }
                }
                if auth.bridge_store.validate_token(t).await {
                    return Ok(next.run(request).await);
                }
            }
            Err(StatusCode::UNAUTHORIZED)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

pub async fn run_http_server(
    ctx: Arc<ToolContext>,
    listen_addr: &str,
    key_store: Arc<ApiKeyStore>,
    bridge_store: Arc<crate::bridge_store::BridgeStore>,
    static_dir: Option<std::path::PathBuf>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
) -> anyhow::Result<()> {
    let config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .disable_allowed_hosts();

    let key_store_for_auth = Arc::clone(&key_store);
    let service = StreamableHttpService::new(
        move || {
            Ok(YunyingServer {
                ctx: Arc::clone(&ctx),
                key_store: Arc::clone(&key_store),
            })
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let download_tokens: DownloadTokenMap =
        Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

    let mut app = Router::new().nest_service("/mcp", service);

    if let Some(dir) = static_dir {
        let dir = Arc::new(dir);
        let dir1 = Arc::clone(&dir);
        let dir2 = Arc::clone(&dir);
        let dir3 = Arc::clone(&dir);
        let dir4 = Arc::clone(&dir);
        app = app
            .route(
                "/install.sh",
                axum::routing::get(move || serve_static(Arc::clone(&dir1), "install.sh")),
            )
            .route(
                "/ca.crt",
                axum::routing::get(move || serve_static(Arc::clone(&dir2), "ca.crt")),
            )
            .route(
                "/releases/{*path}",
                axum::routing::get(
                    move |axum::extract::Path(path): axum::extract::Path<String>| {
                        let dir = Arc::clone(&dir3);
                        async move { serve_static(dir, &format!("releases/{path}")).await }
                    },
                ),
            )
            .route(
                "/recordings/{*path}",
                axum::routing::get(
                    move |axum::extract::Path(path): axum::extract::Path<String>| {
                        let dir = Arc::clone(&dir4);
                        async move { serve_static(dir, &format!("recordings/{path}")).await }
                    },
                ),
            );
    }

    let admin_tokens = Arc::clone(&download_tokens);
    app = app.route(
        "/admin/download-token",
        axum::routing::post(move || {
            let tokens = Arc::clone(&admin_tokens);
            async move {
                use sha2::{Digest, Sha256};
                let mut bytes = [0u8; 16];
                getrandom::getrandom(&mut bytes).expect("CSPRNG failed");
                let token = format!("dl_{}", hex::encode(bytes));
                let hash = hex::encode(Sha256::digest(token.as_bytes()));
                let expires = std::time::Instant::now() + std::time::Duration::from_secs(3600);
                tokens.write().await.insert(hash, expires);
                axum::Json(serde_json::json!({"token": token, "expires_in_secs": 3600}))
            }
        }),
    );

    let auth_state = AuthState {
        store: key_store_for_auth,
        download_tokens,
        bridge_store,
    };
    let app = app.layer(middleware::from_fn_with_state(auth_state, auth_middleware));

    match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => {
            let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                std::path::PathBuf::from(&cert),
                std::path::PathBuf::from(&key),
            )
            .await?;
            let addr: std::net::SocketAddr = listen_addr.parse()?;
            tracing::info!("yunying-mcp HTTPS server listening on {listen_addr}");
            axum_server::bind_rustls(addr, rustls_config)
                .serve(app.into_make_service())
                .await?;
        }
        _ => {
            let listener = tokio::net::TcpListener::bind(listen_addr).await?;
            tracing::info!("yunying-mcp HTTP server listening on {listen_addr} (no TLS)");
            axum::serve(listener, app).await?;
        }
    }
    Ok(())
}

async fn serve_static(
    dir: Arc<std::path::PathBuf>,
    path: &str,
) -> Result<axum::response::Response, StatusCode> {
    let requested = dir.join(path);
    let canonical_dir = dir.canonicalize().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    match requested.canonicalize() {
        Ok(canonical_path) if canonical_path.starts_with(&canonical_dir) => {
            match tokio::fs::read(&canonical_path).await {
                Ok(data) => {
                    let content_type = if path.ends_with(".sh") {
                        "text/x-shellscript"
                    } else if path.ends_with(".crt") || path.ends_with(".pem") {
                        "application/x-pem-file"
                    } else {
                        "application/octet-stream"
                    };
                    Ok(axum::response::Response::builder()
                        .header("content-type", content_type)
                        .body(axum::body::Body::from(data))
                        .unwrap())
                }
                Err(_) => Err(StatusCode::NOT_FOUND),
            }
        }
        _ => {
            // canonicalize failed — either the file doesn't exist or it's
            // a path traversal attempt. Check the parent directory to
            // distinguish: if parent is inside the static dir, return 404
            // (missing file); otherwise 403 (forbidden traversal).
            if let Some(parent) = requested.parent() {
                if let Ok(p) = parent.canonicalize() {
                    if p.starts_with(&canonical_dir) || p == canonical_dir {
                        return Err(StatusCode::NOT_FOUND);
                    }
                }
            }
            Err(StatusCode::FORBIDDEN)
        }
    }
}
