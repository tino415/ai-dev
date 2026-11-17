pub mod auth;
pub mod core;
pub mod html;
pub mod mcp;
pub mod model;
pub mod notebook_file;
pub mod tmux;
pub mod vars;
pub mod web;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::auth::{bearer, Auth};
use crate::core::Core;

pub fn app_router(core: Arc<Core>, auth: Arc<Auth>, allowed_hosts: Vec<String>) -> Router {
    let mcp_core = Arc::clone(&core);
    let mcp_service: StreamableHttpService<mcp::Mcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(mcp::Mcp::new(Arc::clone(&mcp_core))),
        Default::default(),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts(allowed_hosts)
            .with_json_response(true),
    );
    let mcp = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(Arc::clone(&auth), require_static_token));
    web::router(web::App { core, auth }).merge(mcp)
}

async fn require_static_token(State(auth): State<Arc<Auth>>, request: Request, next: Next) -> Response {
    match bearer(request.headers()) {
        Some(token) if auth.static_token_valid(&token) => next.run(request).await,
        _ => (StatusCode::UNAUTHORIZED, "missing or wrong bearer token").into_response(),
    }
}
