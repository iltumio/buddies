mod activity;
mod async_storage;
mod identity;
mod memory;
mod node;
mod pending;
mod protocol;
mod room;
mod server;
mod skill;
mod storage;
mod ticket;
mod validation;
mod watcher;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::future::{Future, IntoFuture};
use std::time::Duration;

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
    let agent_name = std::env::var("BUDDIES_AGENT").unwrap_or_else(|_| "unknown-agent".into());
    let data_path = std::env::var("BUDDIES_DATA_DIR")
        .map(PathBuf::from)
        .ok()
        .or_else(|| Some(default_data_dir()));

    let identity_path = data_path.clone();
    let signer = match tokio::task::spawn_blocking(move || {
        discover_startup_identity(identity_path.as_deref())
    })
    .await?
    {
        Ok(signer) => signer,
        // An explicitly requested signer that fails to initialize is a hard
        // error; only the implicit git default may degrade to unsigned.
        Err(e) if std::env::var("BUDDIES_SIGNER").is_ok() => {
            anyhow::bail!("failed to initialize signing identity from BUDDIES_SIGNER: {e}");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to discover git signing identity; running unsigned");
            None
        }
    };

    let node = Arc::new(
        BuddiesNode::new(BuddiesNodeConfig {
            user_name,
            agent_name,
            signer,
            data_dir: data_path,
        })
        .await?,
    );

    let transport = std::env::var("BUDDIES_TRANSPORT").unwrap_or_else(|_| "stdio".into());

    let result: Result<()> = async {
        match transport.as_str() {
            "http" => {
                let port: u16 = std::env::var("BUDDIES_PORT")
                    .unwrap_or_else(|_| "8080".into())
                    .parse()
                    .context("invalid BUDDIES_PORT")?;
                let host = std::env::var("BUDDIES_HOST").unwrap_or_else(|_| "127.0.0.1".into());
                let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
                eprintln!(
                    "buddies MCP server listening on http://{}/mcp",
                    listener.local_addr()?
                );
                serve_http(node.clone(), listener, shutdown_signal()).await?;
            }
            "stdio" => {
                let service = BuddiesServer::new(node.clone()).serve(stdio()).await?;
                tokio::select! {
                    result = service.waiting() => { result?; }
                    _ = shutdown_signal() => {}
                }
            }
            _ => anyhow::bail!("BUDDIES_TRANSPORT must be stdio or http"),
        }
        Ok(())
    }
    .await;
    let shutdown = node.shutdown().await;
    result?;
    shutdown
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn serve_http(
    node: Arc<BuddiesNode>,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let ct = node.shutdown_token.clone();
    let service = StreamableHttpService::new(
        move || Ok(BuddiesServer::new(node.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(true)
            .with_cancellation_token(ct.child_token()),
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let serve = axum::serve(listener, app)
        .with_graceful_shutdown(ct.clone().cancelled_owned())
        .into_future();
    tokio::pin!(serve);
    tokio::select! {
        result = &mut serve => { result?; }
        _ = shutdown => {
            ct.cancel();
            // A client may keep an HTTP/SSE connection open indefinitely.
            // Give sessions time to finish, then drop outstanding requests.
            if let Ok(result) = tokio::time::timeout(Duration::from_secs(5), &mut serve).await {
                result?;
            }
        }
    }
    Ok(())
}
