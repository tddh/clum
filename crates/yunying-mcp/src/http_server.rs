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

#[derive(Clone)]
struct AuthState {
    store: Arc<ApiKeyStore>,
    agent_name: Arc<std::sync::Mutex<String>>,
}

#[derive(Clone)]
pub struct YunyingServer {
    ctx: Arc<ToolContext>,
    agent_name: Arc<std::sync::Mutex<String>>,
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
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResponse, rmcp::ErrorData>> + Send + '_
    {
        let ctx = Arc::clone(&self.ctx);
        let agent_name = Arc::clone(&self.agent_name);
        let tool_name = request.name.to_string();
        let args = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Object(Default::default()));

        async move {
            if let Ok(name) = agent_name.lock() {
                *ctx.agent_name.lock().unwrap_or_else(|e| e.into_inner()) = name.clone();
            }
            let mut reporter = crate::progress::ProgressReporter::noop();
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
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if auth.store.is_empty().await {
        return Ok(next.run(request).await);
    }

    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    match token {
        Some(t) => match auth.store.validate(t).await {
            Some(identity) => {
                if let Ok(mut name) = auth.agent_name.lock() {
                    *name = identity.name;
                }
                Ok(next.run(request).await)
            }
            None => Err(StatusCode::UNAUTHORIZED),
        },
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

pub async fn run_http_server(
    ctx: Arc<ToolContext>,
    listen_addr: &str,
    key_store: Arc<ApiKeyStore>,
    static_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .disable_allowed_hosts();

    let agent_name: Arc<std::sync::Mutex<String>> =
        Arc::new(std::sync::Mutex::new("unknown".to_string()));
    let agent_name_for_handler = Arc::clone(&agent_name);

    let service = StreamableHttpService::new(
        move || {
            Ok(YunyingServer {
                ctx: Arc::clone(&ctx),
                agent_name: Arc::clone(&agent_name_for_handler),
            })
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );

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

    let auth_state = AuthState {
        store: key_store,
        agent_name,
    };
    let app = app.layer(middleware::from_fn_with_state(auth_state, auth_middleware));

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    tracing::info!("yunying-mcp HTTP server listening on {listen_addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_static(
    dir: Arc<std::path::PathBuf>,
    path: &str,
) -> Result<axum::response::Response, StatusCode> {
    let file_path = dir.join(path);
    if !file_path.starts_with(dir.as_ref()) {
        return Err(StatusCode::FORBIDDEN);
    }
    match tokio::fs::read(&file_path).await {
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
