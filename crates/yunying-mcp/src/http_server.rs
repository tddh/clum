use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
};
use rmcp::{
    RoleServer, ServerHandler,
    model::*,
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::local::LocalSessionManager,
    },
};

use crate::tools::{self, ToolContext};

#[derive(Clone)]
pub struct YunyingServer {
    ctx: Arc<ToolContext>,
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
    ) -> impl std::future::Future<Output = Result<ListToolsResult, rmcp::ErrorData>> + Send + '_ {
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
        let tool_name = request.name.to_string();
        let args = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Object(Default::default()));

        async move {
            let mut reporter = crate::progress::ProgressReporter::noop();
            match tools::execute_tool(&ctx, &tool_name, args, &mut reporter).await {
                Ok(mut result) => {
                    crate::error::enrich_error(&mut result);
                    let is_error = result.get("ok").and_then(|v| v.as_bool()) == Some(false);
                    let text = result.to_string();
                    let mut call_result =
                        CallToolResult::success(vec![ContentBlock::text(text)]);
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
                        let call_result = CallToolResult::error(vec![ContentBlock::text(
                            result.to_string(),
                        )]);
                        Ok(CallToolResponse::Complete(call_result))
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
struct AuthState {
    api_keys: Arc<Vec<String>>,
}

async fn auth_middleware(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if auth.api_keys.is_empty() {
        return Ok(next.run(request).await);
    }

    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    match token {
        Some(t) if auth.api_keys.iter().any(|k| k == t) => Ok(next.run(request).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

pub async fn run_http_server(
    ctx: Arc<ToolContext>,
    listen_addr: &str,
    api_keys: Vec<String>,
) -> anyhow::Result<()> {
    let config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .disable_allowed_hosts();

    let service = StreamableHttpService::new(
        move || Ok(YunyingServer { ctx: Arc::clone(&ctx) }),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let auth_state = AuthState {
        api_keys: Arc::new(api_keys),
    };

    let app = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(auth_state, auth_middleware));

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    tracing::info!("yunying-mcp HTTP server listening on {listen_addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
