mod activity;
mod async_storage;
mod identity;
mod local;
mod memory;
mod monitor;
mod node;
mod pending;
mod protocol;
mod resilience;
mod room;
mod server;
mod sessions;
mod skill;
mod status;
mod storage;
mod ticket;
mod validation;
mod watcher;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
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

#[derive(Parser)]
#[command(version, about = "P2P communication for AI agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Monitor a running buddies HTTP server without opening its database.
    Monitor {
        /// Server URL (the /mcp suffix is also accepted).
        #[arg(long, env = "BUDDIES_URL", default_value = "http://127.0.0.1:8080")]
        url: String,
        /// Print one JSON snapshot instead of opening the terminal UI.
        #[arg(long)]
        once: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Command::Monitor { url, once }) = cli.command {
        return monitor::run(&url, once).await;
    }
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
            presence: resilience::PresenceConfig::from_env()?,
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
    let status_node = node.clone();
    let sessions = sessions::Sessions::new(resilience::env_duration("BUDDIES_MCP_IDLE_SECS", 300)?);
    let maintenance_node = node.clone();
    let maintenance_sessions = sessions.clone();
    let maintenance = async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            maintenance_sessions.sweep().await;
            maintenance_node.clients.prune();
        }
    };
    let service = StreamableHttpService::new(
        move || Ok(BuddiesServer::new(node.clone())),
        sessions.manager.clone(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(true)
            .with_cancellation_token(ct.child_token()),
    );
    let app = axum::Router::new()
        .route("/status", axum::routing::get(status::snapshot))
        .with_state(status_node)
        .nest_service(
            "/mcp",
            axum::Router::new().fallback_service(service).layer(
                axum::middleware::from_fn_with_state(sessions, sessions::activity),
            ),
        );
    let serve = axum::serve(listener, app)
        .with_graceful_shutdown(ct.clone().cancelled_owned())
        .into_future();
    tokio::pin!(serve);
    tokio::select! {
        _ = maintenance => unreachable!("session maintenance does not terminate"),
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
