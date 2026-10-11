//! A stdio Claude Channel backed by the shared HTTP service, not another node/database.
use anyhow::{Result, ensure};
use clap::Args;
use rmcp::{
    ClientHandler, ErrorData, Peer, RoleClient, RoleServer, ServerHandler, ServiceExt,
    model::*,
    service::{NotificationContext, RequestContext, RunningService},
    transport::{StreamableHttpClientTransport, stdio},
};
use serde_json::{Value, json};
use std::{borrow::Cow, collections::HashSet, time::Duration};
use tokio::sync::broadcast;

pub const INSTRUCTIONS: &str = "Collaborate on the configured room's goal. Your stable participant name is already registered. State your responsibility and consult peers before changing shared contracts. Messages arrive with room, from, kind, id and conversation_id. For a question or request, inspect get_conversation, acknowledge_message as read, act within the user's authorized task, then send_message to the sender with in_reply_to and acknowledge as handled or failed. Report blockers explicitly. Answers continue existing work; updates are context and do not require a reply. Do not create acknowledgement ping-pong. Task events require submit_task_result. A written notification is not proof of reading. Keep the configured participant name and room.";

#[derive(Args, Clone)]
#[group(id = "channel-connection")]
pub struct Options {
    #[arg(long)]
    pub room: String,
    /// Stable, room-unique participant name, e.g. claude-backend.
    #[arg(long)]
    pub name: String,
    #[arg(long, env = "BUDDIES_URL", default_value = "http://127.0.0.1:8080")]
    pub url: String,
}

pub struct Events(broadcast::Sender<CustomNotification>);
impl ClientHandler for Events {
    fn get_info(&self) -> ClientConfig {
        let mut config = ClientConfig::default();
        config.client_info = Implementation::new("buddies-channel", env!("CARGO_PKG_VERSION"));
        config
    }
    async fn on_custom_notification(
        &self,
        event: CustomNotification,
        _: NotificationContext<RoleClient>,
    ) {
        let _ = self.0.send(event);
    }
}

pub struct Connection {
    pub service: RunningService<RoleClient, Events>,
    pub events: broadcast::Receiver<CustomNotification>,
}
pub async fn connect(options: &Options, delivery: &str, accepts_tasks: bool) -> Result<Connection> {
    crate::conversation::validate_name(&options.name)?;
    let mut url = reqwest::Url::parse(&options.url)?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "service URL must be HTTP(S)"
    );
    if url.path().trim_end_matches('/') != "/mcp" {
        url.set_path("/mcp");
    }
    url.set_query(None);
    url.set_fragment(None);
    let (tx, events) = broadcast::channel(64);
    let transport = StreamableHttpClientTransport::from_uri(url.to_string());
    let service = Events(tx).serve(transport).await?;
    call(&service, "join_room", json!({"room":options.room})).await?;
    call(
        &service,
        "register_participant",
        json!({"room":options.room,"name":options.name,
        "delivery":delivery,"accepts_tasks":accepts_tasks}),
    )
    .await?;
    Ok(Connection { service, events })
}
pub async fn call(peer: &Peer<RoleClient>, name: &str, args: Value) -> Result<Value> {
    let result = peer
        .call_tool(
            CallToolRequestParams::new(name.to_owned())
                .with_arguments(args.as_object().cloned().unwrap_or_default()),
        )
        .await?;
    ensure!(
        result.is_error != Some(true),
        "buddies {name}: {:?}",
        result.content
    );
    let value = serde_json::to_value(&result)?;
    let text = value["content"][0]["text"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing tool result"))?;
    Ok(serde_json::from_str(text)?)
}

struct Channel {
    upstream: Peer<RoleClient>,
    options: Options,
    events: std::sync::Mutex<Option<broadcast::Receiver<CustomNotification>>>,
    stop: tokio_util::sync::CancellationToken,
}
impl ServerHandler for Channel {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }
    fn get_info(&self) -> ServerConfig {
        let mut capabilities = ServerCapabilities::builder().enable_tools().build();
        capabilities.experimental =
            Some(serde_json::from_value(json!({"claude/channel":{}})).unwrap());
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("buddies", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "{INSTRUCTIONS} Room: {}. Participant: {}.",
                self.options.room, self.options.name
            ))
    }
    async fn list_tools(
        &self,
        req: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.upstream
            .list_tools(req)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }
    async fn call_tool(
        &self,
        req: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if req
            .arguments
            .as_ref()
            .and_then(|a| a.get("room"))
            .and_then(Value::as_str)
            .is_some_and(|r| r != self.options.room)
        {
            return Err(ErrorData::invalid_params(
                "channel is bound to its configured room",
                None,
            ));
        }
        if req.name == "leave_room" || req.name == "register_participant" {
            return Err(ErrorData::invalid_params(
                "channel manages its own membership and participant identity",
                None,
            ));
        }
        self.upstream
            .call_tool(req)
            .await
            .map(Into::into)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let events = self.events.lock().unwrap().take();
        if let Some(events) = events {
            let upstream = self.upstream.clone();
            let options = self.options.clone();
            let stop = self.stop.clone();
            tokio::spawn(async move {
                let run = async {
                    call(
                        &upstream,
                        "register_participant",
                        json!({"room":options.room,
                        "name":options.name,"delivery":"channel","accepts_tasks":true}),
                    )
                    .await?;
                    forward(upstream, context.peer, options, events).await
                };
                let result =
                    tokio::select! { result = run => result, _ = stop.cancelled() => Ok(()) };
                if let Err(e) = result {
                    eprintln!("buddies channel stopped: {e}");
                }
                stop.cancel();
            });
        }
    }
}

async fn emit(peer: &Peer<RoleServer>, kind: &str, room: &str, value: Value) -> Result<()> {
    let params =
        json!({"content":serde_json::to_string(&value)?, "meta":{"room":room,"event":kind}});
    tokio::time::timeout(
        Duration::from_secs(5),
        peer.send_notification(ServerNotification::CustomNotification(
            CustomNotification::new("notifications/claude/channel", Some(params)),
        )),
    )
    .await??;
    Ok(())
}
async fn forward(
    upstream: Peer<RoleClient>,
    downstream: Peer<RoleServer>,
    options: Options,
    mut events: broadcast::Receiver<CustomNotification>,
) -> Result<()> {
    let mut sent = HashSet::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if downstream.is_transport_closed() || upstream.is_transport_closed() { return Ok(()); }
                let inbox = call(&upstream, "poll_messages", json!({"room":options.room})).await?;
                let messages = inbox["messages"].as_array().ok_or_else(|| anyhow::anyhow!("missing inbox"))?;
                let ids: HashSet<_> = messages.iter().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect();
                sent.retain(|id| ids.contains(id));
                for message in messages {
                    let id = message["id"].as_str().ok_or_else(|| anyhow::anyhow!("missing message id"))?;
                    if !sent.contains(id) {
                        emit(&downstream, "message", &options.room, message.clone()).await?;
                        sent.insert(id.to_owned());
                    }
                }
                let tasks = call(&upstream, "poll_pending_tasks", json!({"room":options.room,"wait_secs":0})).await?;
                for task in tasks["tasks"].as_array().into_iter().flatten() {
                    emit(&downstream, "task", &options.room, task.clone()).await?;
                }
            }
            event = events.recv() => match event {
                Ok(event) if event.method == "notifications/buddies/status" || event.method == "notifications/buddies/fileConflict" => {
                    emit(&downstream, &event.method, &options.room, event.params.unwrap_or(Value::Null)).await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
                Err(broadcast::error::RecvError::Lagged(n)) => eprintln!("buddies channel skipped {n} transient events; durable inbox retained"),
                _ => {}
            }
        }
    }
}
pub async fn run(options: Options) -> Result<()> {
    let connection = connect(&options, "channel", false).await?;
    let stop = tokio_util::sync::CancellationToken::new();
    let server = Channel {
        upstream: connection.service.peer().clone(),
        options,
        events: std::sync::Mutex::new(Some(connection.events)),
        stop: stop.clone(),
    }
    .serve(stdio())
    .await?;
    tokio::select! {
        result = server.waiting() => { result?; }
        _ = crate::shutdown_signal() => {}
        _ = stop.cancelled() => {}
    }
    stop.cancel();
    connection.service.cancel().await?;
    Ok(())
}
