mod identity;
mod memory;
mod node;
mod protocol;
mod room;
mod server;
mod skill;
mod storage;
mod ticket;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use crate::identity::discover_startup_identity;
use crate::node::{BuddiesNode, BuddiesNodeConfig};
use crate::server::BuddiesServer;

fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .map(|d| d.join("buddies"))
        .unwrap_or_else(|| PathBuf::from(".buddies"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let user_name = std::env::var("BUDDIES_USER")
        .unwrap_or_else(|_| whoami::username().unwrap_or_else(|_| "anonymous".into()));
    let agent_name =
        std::env::var("BUDDIES_AGENT").unwrap_or_else(|_| "unknown-agent".into());
    let data_path = std::env::var("BUDDIES_DATA_DIR")
        .map(PathBuf::from)
        .ok()
        .or_else(|| Some(default_data_dir()));

    let node = Arc::new(
        BuddiesNode::new(BuddiesNodeConfig {
            user_name,
            agent_name,
            signer: discover_startup_identity(data_path.as_deref()).ok().flatten(),
            data_dir: data_path,
        })
        .await?,
    );

    let transport = std::env::var("BUDDIES_TRANSPORT")
        .unwrap_or_else(|_| "stdio".into());

    match transport.as_str() {
        "http" => {
            let port: u16 = std::env::var("BUDDIES_PORT")
                .unwrap_or_else(|_| "8080".into())
                .parse()
                .expect("BUDDIES_PORT must be a valid port number");
            let bind_addr = std::env::var("BUDDIES_HOST")
                .unwrap_or_else(|_| "127.0.0.1".into());
            let addr = format!("{bind_addr}:{port}");

            let ct = tokio_util::sync::CancellationToken::new();
            let service = StreamableHttpService::new(
                move || Ok(BuddiesServer::new(Arc::clone(&node))),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig {
                    stateful_mode: true,
                    cancellation_token: ct.child_token(),
                    ..Default::default()
                },
            );

            let app = axum::Router::new().nest_service("/mcp", service);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            tracing::info!("buddies HTTP server listening on {addr}");
            eprintln!("buddies MCP server listening on http://{addr}/mcp");
            axum::serve(listener, app).await?;
        }
        _ => {
            let server = BuddiesServer::new(Arc::clone(&node));
            let service = server.serve(stdio()).await?;
            service.waiting().await?;
            node.shutdown().await?;
        }
    }

    Ok(())
}
