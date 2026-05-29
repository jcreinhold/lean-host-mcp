//! Private Streamable HTTP transport wiring for the binary.
//!
//! The MCP service and project broker stay transport-agnostic. This module
//! owns the axum/rmcp session plumbing needed to expose the same service over
//! Streamable HTTP.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use lean_host_mcp::{LeanHostService, ProjectBroker};

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct HttpServeConfig {
    pub(crate) bind: SocketAddr,
    pub(crate) path: String,
}

#[allow(
    clippy::significant_drop_tightening,
    reason = "the HTTP service and router intentionally live until axum::serve returns"
)]
pub(crate) async fn serve(broker: Arc<ProjectBroker>, config: HttpServeConfig) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("bind Streamable HTTP listener at {}", config.bind))?;
    let local_addr = listener.local_addr().context("read Streamable HTTP listener address")?;

    let cancellation = CancellationToken::new();
    let http_config = StreamableHttpServerConfig::default().with_cancellation_token(cancellation.child_token());
    let service = StreamableHttpService::new(
        move || Ok(LeanHostService::new(Arc::clone(&broker))),
        Arc::new(LocalSessionManager::default()),
        http_config,
    );
    let router = Router::new().nest_service(&config.path, service);

    tracing::info!(
        bind = %local_addr,
        http_path = %config.path,
        "Streamable HTTP transport listening",
    );

    let shutdown_token = cancellation.clone();
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %err, "failed to wait for shutdown signal");
        }
        shutdown_token.cancel();
    });

    let result = server.await.context("serve Streamable HTTP transport");
    cancellation.cancel();
    result
}
