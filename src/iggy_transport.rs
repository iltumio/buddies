//! Persistent message-streaming transport powered by Apache Iggy (`iggy-rs`).
//!
//! Maps `buddies` rooms to topics inside an Iggy stream, attaches structured
//! Iggy `HeaderKey`/`HeaderValue` metadata (repository, branch, worktree, agent,
//! task_id, sprint, path, and affected-files count) to every published message,
//! and supports offset-based log replay for agents joining an active room.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use iggy::bytes_serializable::BytesSerializable;
use iggy::client::{Client, MessageClient, StreamClient, TopicClient};
use iggy::clients::client::IggyClient;
use iggy::compression::compression_algorithm::CompressionAlgorithm;
use iggy::consumer::Consumer;
use iggy::identifier::Identifier;
use iggy::messages::poll_messages::PollingStrategy;
use iggy::messages::send_messages::{Message as IggyWireMessage, Partitioning};
use iggy::models::header::{HeaderKey, HeaderValue};
use iggy::models::messages::{MessageState, PolledMessage};
use iggy::utils::byte_size::IggyByteSize;
use iggy::utils::expiry::IggyExpiry;
use iggy::utils::topic_size::MaxTopicSize;
use iroh::{EndpointAddr, EndpointId};
use tokio::sync::{Mutex, Notify, mpsc};
use tracing::{debug, warn};

use crate::protocol::{P2PMessage, P2PMessageBody, TopicId};
use crate::transport::{
    BoxFuture, RoomReceiver, RoomSender, RoomSubscription, RoomTransport, TransportEvent,
};

const MAX_HEADER_STR_BYTES: usize = 255;
const DEFAULT_STREAM_NAME: &str = "buddies";
const DEFAULT_POLL_INTERVAL_MS: u64 = 40;
const POLL_BATCH_COUNT: u32 = 64;

/// Configuration for the Apache Iggy streaming transport.
#[derive(Clone, Debug)]
pub struct IggyConfig {
    /// Stream name in Apache Iggy (default: `"buddies"`).
    pub stream_name: String,
    /// Number of partitions per room topic (default: `1`).
    pub partitions_count: u32,
    /// Poll interval in milliseconds for stream consumers.
    pub poll_interval: Duration,
    /// Whether new room subscriptions replay existing stream history from offset 0.
    pub replay_from_start: bool,
    /// Backend execution mode (live `iggy-server` connection or embedded Iggy broker).
    pub mode: IggyBackendMode,
}

#[derive(Clone)]
pub enum IggyBackendMode {
    /// Connect to an external `iggy-server` daemon via connection string
    /// (e.g. `iggy://iggy:iggy@127.0.0.1:8090`, `iggy+quic://...`, `iggy+http://...`).
    Server { connection_string: String },
    /// Embedded in-memory Iggy stream broker using `iggy` wire messages, headers, and offsets.
    Memory(IggyMemoryBroker),
}

impl std::fmt::Debug for IggyBackendMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server { .. } => f.write_str("Server { connection_string: [redacted] }"),
            Self::Memory(_) => f.write_str("Memory"),
        }
    }
}

impl Default for IggyConfig {
    fn default() -> Self {
        Self {
            stream_name: DEFAULT_STREAM_NAME.to_string(),
            partitions_count: 1,
            poll_interval: Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
            replay_from_start: true,
            mode: IggyBackendMode::Server {
                connection_string: "iggy://iggy:iggy@127.0.0.1:8090".into(),
            },
        }
    }
}

impl IggyConfig {
    #[cfg(test)]
    pub fn in_memory(broker: IggyMemoryBroker) -> Self {
        Self {
            mode: IggyBackendMode::Memory(broker),
            ..Self::default()
        }
    }

    pub fn from_env() -> Result<Self> {
        let stream_name = std::env::var("BUDDIES_IGGY_STREAM")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_STREAM_NAME.to_string());

        let partitions_count: u32 = std::env::var("BUDDIES_IGGY_PARTITIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
            .max(1);

        let poll_ms: u64 = std::env::var("BUDDIES_IGGY_POLL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_POLL_INTERVAL_MS)
            .clamp(5, 5_000);

        let replay_from_start = std::env::var("BUDDIES_IGGY_REPLAY")
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "false" | "0" | "latest"
                )
            })
            .unwrap_or(true);

        let url = std::env::var("BUDDIES_IGGY_URL")
            .or_else(|_| std::env::var("BUDDIES_IGGY_CONNECTION_STRING"))
            .or_else(|_| std::env::var("IGGY_CONNECTION_STRING"))
            .context(
                "Iggy requires BUDDIES_IGGY_URL (use memory explicitly for process-local tests)",
            )?;

        anyhow::ensure!(
            partitions_count == 1,
            "Buddies currently requires exactly one Iggy partition per room"
        );
        let mode = if matches!(
            url.trim().to_ascii_lowercase().as_str(),
            "memory" | "in-memory" | "local"
        ) {
            IggyBackendMode::Memory(IggyMemoryBroker::global())
        } else {
            let conn = if url.contains("://") {
                url
            } else {
                let user = std::env::var("BUDDIES_IGGY_USERNAME").unwrap_or_else(|_| "iggy".into());
                let pass = std::env::var("BUDDIES_IGGY_PASSWORD").unwrap_or_else(|_| "iggy".into());
                let proto = std::env::var("BUDDIES_IGGY_PROTOCOL")
                    .ok()
                    .filter(|p| !p.trim().is_empty())
                    .map(|p| format!("+{p}"))
                    .unwrap_or_default();
                format!("iggy{proto}://{user}:{pass}@{}", url.trim())
            };
            IggyBackendMode::Server {
                connection_string: conn,
            }
        };

        Ok(Self {
            stream_name,
            partitions_count,
            poll_interval: Duration::from_millis(poll_ms),
            replay_from_start,
            mode,
        })
    }
}

/// Truncate a string on a UTF-8 char boundary so it fits Iggy's 255-byte `HeaderValue` constraint.
fn cap_header_str(value: &str) -> &str {
    if value.len() <= MAX_HEADER_STR_BYTES {
        return value;
    }
    let mut cut = MAX_HEADER_STR_BYTES;
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    &value[..cut]
}

fn insert_str_header(headers: &mut HashMap<HeaderKey, HeaderValue>, key: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    let capped = cap_header_str(value);
    if let (Ok(k), Ok(v)) = (HeaderKey::new(key), HeaderValue::from_str(capped)) {
        headers.insert(k, v);
    }
}

fn insert_u32_header(headers: &mut HashMap<HeaderKey, HeaderValue>, key: &str, value: u32) {
    if let (Ok(k), Ok(v)) = (HeaderKey::new(key), HeaderValue::from_uint32(value)) {
        headers.insert(k, v);
    }
}

/// Extract structured Iggy headers (`HeaderKey` -> `HeaderValue`) from a serialized `P2PMessage`.
///
/// This enables stream consumers, external tooling, and offset filters to inspect the
/// repository, branch, worktree, agent, task_id, sprint, file path, and affected-file blast
/// radius directly from Iggy message headers without decoding the body.
pub fn build_iggy_headers(
    sender_id: EndpointId,
    payload: &Bytes,
) -> HashMap<HeaderKey, HeaderValue> {
    let mut headers = HashMap::new();
    insert_str_header(&mut headers, "buddies.sender_id", &sender_id.to_string());

    let Ok(msg) = P2PMessage::from_bytes(payload) else {
        insert_str_header(&mut headers, "buddies.msg_kind", "raw");
        return headers;
    };

    if let Some(signer) = &msg.signed_by {
        insert_str_header(&mut headers, "buddies.signed_by", &signer.to_label());
    }

    match &msg.body {
        P2PMessageBody::FileActivity { entry } | P2PMessageBody::FileActivityV2 { entry } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "file_activity");
            insert_str_header(&mut headers, "buddies.repo", &entry.repo);
            insert_str_header(&mut headers, "buddies.branch", &entry.branch);
            if let Some(wt) = &entry.worktree {
                insert_str_header(&mut headers, "buddies.worktree", wt);
            }
            insert_str_header(&mut headers, "buddies.agent", &entry.agent);
            insert_str_header(&mut headers, "buddies.author", &entry.author);
            if let Some(task_id) = &entry.task_id {
                insert_str_header(&mut headers, "buddies.task_id", task_id);
            }
            if let Some(sprint) = &entry.sprint {
                insert_str_header(&mut headers, "buddies.sprint", sprint);
            }
            insert_str_header(&mut headers, "buddies.path", &entry.path);
            insert_str_header(
                &mut headers,
                "buddies.impact_truncated",
                if entry.impact_truncated {
                    "true"
                } else {
                    "false"
                },
            );
            insert_str_header(&mut headers, "buddies.change_kind", &entry.kind.to_string());
            insert_u32_header(
                &mut headers,
                "buddies.affected_count",
                entry.affected_files.len() as u32,
            );
            if !entry.affected_files.is_empty() {
                let preview = entry.affected_files.join(",");
                insert_str_header(&mut headers, "buddies.affected_files", &preview);
            }
            if !entry.touched_symbols.is_empty() {
                let symbols = entry.touched_symbols.join(",");
                insert_str_header(&mut headers, "buddies.touched_symbols", &symbols);
            }
        }
        P2PMessageBody::NegotiationProposed { proposal } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "negotiation_proposed");
            insert_str_header(
                &mut headers,
                "buddies.proposal_id",
                &proposal.id.to_string(),
            );
            insert_str_header(&mut headers, "buddies.repo", &proposal.repo);
            insert_str_header(&mut headers, "buddies.branch", &proposal.branch);
            if let Some(wt) = &proposal.worktree {
                insert_str_header(&mut headers, "buddies.worktree", wt);
            }
            insert_str_header(&mut headers, "buddies.agent", &proposal.agent);
            insert_str_header(&mut headers, "buddies.proposer", &proposal.proposer);
            if let Some(task_id) = &proposal.task_id {
                insert_str_header(&mut headers, "buddies.task_id", task_id);
            }
            if let Some(sprint) = &proposal.sprint {
                insert_str_header(&mut headers, "buddies.sprint", sprint);
            }
            insert_str_header(&mut headers, "buddies.strategy", &proposal.strategy);
        }
        P2PMessageBody::NegotiationResponded {
            room,
            proposal_id,
            response,
        } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "negotiation_responded");
            insert_str_header(&mut headers, "buddies.room", room);
            insert_str_header(
                &mut headers,
                "buddies.proposal_id",
                &proposal_id.to_string(),
            );
            insert_str_header(&mut headers, "buddies.agent", &response.agent);
            insert_str_header(&mut headers, "buddies.responder", &response.responder);
            if let Some(task_id) = &response.task_id {
                insert_str_header(&mut headers, "buddies.task_id", task_id);
            }
            if let Some(sprint) = &response.sprint {
                insert_str_header(&mut headers, "buddies.sprint", sprint);
            }
        }
        P2PMessageBody::Join { name, agent } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "join");
            insert_str_header(&mut headers, "buddies.author", name);
            insert_str_header(&mut headers, "buddies.agent", agent);
        }
        P2PMessageBody::Leave { name } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "leave");
            insert_str_header(&mut headers, "buddies.author", name);
        }
        P2PMessageBody::MemoryCreated { entry } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "memory_created");
            insert_str_header(&mut headers, "buddies.room", &entry.room);
            insert_str_header(&mut headers, "buddies.author", &entry.author);
        }
        P2PMessageBody::StatusUpdate { author, .. } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "status_update");
            insert_str_header(&mut headers, "buddies.author", author);
        }
        P2PMessageBody::SearchRequest { .. } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "search_request");
        }
        P2PMessageBody::SearchResponse { .. } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "search_response");
        }
        P2PMessageBody::TaskRequest {
            task_id,
            source_peer,
            room,
            ..
        } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "task_request");
            insert_str_header(&mut headers, "buddies.task_id", &task_id.to_string());
            insert_str_header(&mut headers, "buddies.author", source_peer);
            insert_str_header(&mut headers, "buddies.room", room);
        }
        P2PMessageBody::TaskClaimed {
            task_id,
            claimed_by,
        } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "task_claimed");
            insert_str_header(&mut headers, "buddies.task_id", &task_id.to_string());
            insert_str_header(&mut headers, "buddies.author", claimed_by);
        }
        P2PMessageBody::TaskResponse {
            task_id,
            completed_by,
            ..
        } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "task_response");
            insert_str_header(&mut headers, "buddies.task_id", &task_id.to_string());
            insert_str_header(&mut headers, "buddies.author", completed_by);
        }
        P2PMessageBody::SkillPublished { entry } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "skill_published");
            insert_str_header(&mut headers, "buddies.room", &entry.room);
            insert_str_header(&mut headers, "buddies.author", &entry.author);
        }
        P2PMessageBody::SkillSearchRequest { .. } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "skill_search_request");
        }
        P2PMessageBody::SkillSearchResponse { .. } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "skill_search_response");
        }
        P2PMessageBody::SkillVoteCast { voter, .. } => {
            insert_str_header(&mut headers, "buddies.msg_kind", "skill_vote_cast");
            insert_str_header(&mut headers, "buddies.author", voter);
        }
    }

    headers
}

fn sender_from_headers(headers: &Option<HashMap<HeaderKey, HeaderValue>>) -> Option<EndpointId> {
    let map = headers.as_ref()?;
    let key = HeaderKey::new("buddies.sender_id").ok()?;
    let val = map.get(&key)?;
    let text = val.as_str().ok()?;
    EndpointId::from_str(text).ok()
}

fn topic_name_for(topic: TopicId) -> String {
    format!("room-{}", data_encoding::HEXLOWER.encode(topic.as_bytes()))
}

// ============================================================================
// Embedded Iggy Stream Broker (Offset-indexed `PolledMessage` partitions)
// ============================================================================

#[derive(Default)]
struct MemoryTopicLog {
    messages: Vec<PolledMessage>,
    active_members: HashSet<EndpointId>,
    notify: Arc<Notify>,
}

/// In-memory Apache Iggy stream broker storing real `PolledMessage` records and headers.
#[derive(Clone, Default)]
pub struct IggyMemoryBroker {
    streams: Arc<Mutex<HashMap<(String, String), MemoryTopicLog>>>,
    next_id: Arc<AtomicU64>,
}

impl std::fmt::Debug for IggyMemoryBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IggyMemoryBroker").finish()
    }
}

impl IggyMemoryBroker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn global() -> Self {
        static GLOBAL: std::sync::OnceLock<IggyMemoryBroker> = std::sync::OnceLock::new();
        GLOBAL.get_or_init(IggyMemoryBroker::new).clone()
    }

    /// Append an `IggyWireMessage` (validating wire serialization roundtrip via `BytesSerializable`)
    /// and return its assigned offset.
    pub async fn send_message(
        &self,
        stream: &str,
        topic: &str,
        wire_msg: IggyWireMessage,
    ) -> Result<u64> {
        // Exercise Iggy's binary wire format serialization to guarantee wire compatibility
        let serialized = wire_msg.to_bytes();
        let decoded = IggyWireMessage::from_bytes(serialized)
            .map_err(|e| anyhow::anyhow!("iggy message wire validation failed: {e}"))?;

        let mut streams = self.streams.lock().await;
        let log = streams
            .entry((stream.to_string(), topic.to_string()))
            .or_default();
        let offset = log.messages.len() as u64;
        let id = if decoded.id == 0 {
            u128::from(self.next_id.fetch_add(1, Ordering::Relaxed) + 1)
        } else {
            decoded.id
        };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;

        let polled = PolledMessage {
            offset,
            state: MessageState::Available,
            timestamp,
            id,
            checksum: 0,
            headers: decoded.headers,
            length: IggyByteSize::from(decoded.payload.len() as u64),
            payload: decoded.payload,
        };
        log.messages.push(polled);
        log.notify.notify_waiters();
        Ok(offset)
    }

    /// Read all messages stored in `(stream, topic)` starting at `start_offset`.
    pub async fn poll_messages(
        &self,
        stream: &str,
        topic: &str,
        start_offset: u64,
        count: usize,
    ) -> Vec<PolledMessage> {
        let streams = self.streams.lock().await;
        let Some(log) = streams.get(&(stream.to_string(), topic.to_string())) else {
            return Vec::new();
        };
        let start = usize::try_from(start_offset).unwrap_or(usize::MAX);
        if start >= log.messages.len() {
            return Vec::new();
        }
        log.messages[start..]
            .iter()
            .take(count)
            .map(clone_polled_message)
            .collect()
    }
}

fn clone_polled_message(m: &PolledMessage) -> PolledMessage {
    PolledMessage {
        offset: m.offset,
        state: m.state,
        timestamp: m.timestamp,
        id: m.id,
        checksum: m.checksum,
        headers: m.headers.clone(),
        length: m.length,
        payload: m.payload.clone(),
    }
}

// ============================================================================
// IggyTransport (`RoomTransport` Implementation)
// ============================================================================

pub struct IggyTransport {
    id: EndpointId,
    config: IggyConfig,
    client: Option<Arc<IggyClient>>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
}

impl IggyTransport {
    pub async fn new(id: EndpointId, config: IggyConfig) -> Result<Self> {
        anyhow::ensure!(
            config.partitions_count == 1,
            "exactly one Iggy partition per room is supported"
        );
        let client = match &config.mode {
            IggyBackendMode::Server { connection_string } => {
                let client =
                    IggyClient::from_connection_string(connection_string).map_err(|_| {
                        anyhow::anyhow!("invalid Iggy connection string (credentials redacted)")
                    })?;
                tokio::time::timeout(Duration::from_secs(5), client.connect())
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("Iggy connection timed out; check server/SDK compatibility")
                    })?
                    .map_err(|_| {
                        anyhow::anyhow!("failed to connect to Iggy server (credentials redacted)")
                    })?;
                let stream_id = Identifier::named(&config.stream_name)
                    .map_err(|e| anyhow::anyhow!("invalid Iggy stream name: {e}"))?;
                if client.get_stream(&stream_id).await?.is_none()
                    && let Err(error) = client.create_stream(&config.stream_name, None).await
                {
                    anyhow::ensure!(
                        client.get_stream(&stream_id).await?.is_some(),
                        "failed to create Iggy stream: {error}"
                    );
                }
                Some(Arc::new(client))
            }
            IggyBackendMode::Memory(_) => None,
        };

        Ok(Self {
            id,
            config,
            client,
            closed: Arc::new(AtomicBool::new(false)),
            closed_notify: Arc::new(Notify::new()),
        })
    }
}

struct IggyRoomSender {
    id: EndpointId,
    stream_name: String,
    topic_name: String,
    mode: IggyBackendMode,
    client: Option<Arc<IggyClient>>,
    closed: Arc<AtomicBool>,
}

impl RoomSender for IggyRoomSender {
    fn broadcast(&self, payload: Bytes) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            anyhow::ensure!(
                !self.closed.load(Ordering::SeqCst),
                "iggy transport is shut down"
            );
            let headers = build_iggy_headers(self.id, &payload);
            let wire_msg = IggyWireMessage::new(None, payload, Some(headers));

            match (&self.mode, &self.client) {
                (IggyBackendMode::Memory(broker), _) => {
                    broker
                        .send_message(&self.stream_name, &self.topic_name, wire_msg)
                        .await?;
                }
                (IggyBackendMode::Server { .. }, Some(client)) => {
                    let stream_id = Identifier::named(&self.stream_name)
                        .map_err(|e| anyhow::anyhow!("invalid stream identifier: {e}"))?;
                    let topic_id = Identifier::named(&self.topic_name)
                        .map_err(|e| anyhow::anyhow!("invalid topic identifier: {e}"))?;
                    let mut messages = [wire_msg];
                    client
                        .send_messages(
                            &stream_id,
                            &topic_id,
                            &Partitioning::partition_id(1),
                            &mut messages,
                        )
                        .await
                        .context("failed to publish message to Iggy topic")?;
                }
                _ => anyhow::bail!("iggy client not initialized"),
            }
            Ok(())
        })
    }

    fn join_peers(&self, _peers: Vec<EndpointId>) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            anyhow::ensure!(
                !self.closed.load(Ordering::SeqCst),
                "iggy transport is shut down"
            );
            Ok(())
        })
    }
}

struct IggyRoomReceiver {
    rx: mpsc::Receiver<TransportEvent>,
    poll_task: tokio::task::JoinHandle<()>,
    membership: Option<(IggyMemoryBroker, String, String, EndpointId)>,
}

impl Drop for IggyRoomReceiver {
    fn drop(&mut self) {
        self.poll_task.abort();
        if let Some((broker, stream, topic, id)) = self.membership.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                if let Some(log) = broker.streams.lock().await.get_mut(&(stream, topic)) {
                    log.active_members.remove(&id);
                    log.notify.notify_waiters();
                }
            });
        }
    }
}

impl RoomReceiver for IggyRoomReceiver {
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<TransportEvent>>> {
        Box::pin(async move { Ok(self.rx.recv().await) })
    }
}

impl RoomTransport for IggyTransport {
    fn node_id(&self) -> EndpointId {
        self.id
    }

    fn endpoint_addr(&self) -> EndpointAddr {
        EndpointAddr::new(self.id)
    }

    fn register_peer_addr(&self, _addr: EndpointAddr) {}

    fn subscribe(
        &self,
        topic: TopicId,
        bootstrap: Vec<EndpointId>,
    ) -> BoxFuture<'_, Result<RoomSubscription>> {
        let id = self.id;
        let config = self.config.clone();
        let client = self.client.clone();
        let closed = Arc::clone(&self.closed);
        let closed_notify = Arc::clone(&self.closed_notify);

        Box::pin(async move {
            anyhow::ensure!(
                !closed.load(Ordering::SeqCst),
                "iggy transport is shut down"
            );
            let stream_name = config.stream_name.clone();
            let topic_name = topic_name_for(topic);

            // Ensure topic exists on the server or memory broker
            let (mut next_offset, history_until, topic_notify, initial_peers) =
                match (&config.mode, &client) {
                    (IggyBackendMode::Memory(broker), _) => {
                        let mut streams = broker.streams.lock().await;
                        let log = streams
                            .entry((stream_name.clone(), topic_name.clone()))
                            .or_default();
                        let start_offset = if config.replay_from_start {
                            0
                        } else {
                            log.messages.len() as u64
                        };
                        let peers: Vec<EndpointId> = log
                            .active_members
                            .iter()
                            .copied()
                            .filter(|&p| p != id)
                            .collect();
                        log.active_members.insert(id);
                        log.notify.notify_waiters();
                        (
                            start_offset,
                            log.messages.len() as u64,
                            Some(Arc::clone(&log.notify)),
                            peers,
                        )
                    }
                    (IggyBackendMode::Server { .. }, Some(client)) => {
                        let stream_id = Identifier::named(&stream_name)
                            .map_err(|e| anyhow::anyhow!("invalid stream name: {e}"))?;
                        let topic_id = Identifier::named(&topic_name)
                            .map_err(|e| anyhow::anyhow!("invalid topic name: {e}"))?;
                        if client.get_topic(&stream_id, &topic_id).await?.is_none() {
                            let creation = client
                                .create_topic(
                                    &stream_id,
                                    &topic_name,
                                    config.partitions_count,
                                    CompressionAlgorithm::None,
                                    None,
                                    None,
                                    IggyExpiry::ServerDefault,
                                    MaxTopicSize::ServerDefault,
                                )
                                .await;
                            if let Err(error) = creation {
                                anyhow::ensure!(
                                    client.get_topic(&stream_id, &topic_id).await?.is_some(),
                                    "failed to create Iggy topic: {error}"
                                );
                            }
                        }
                        let details = client
                            .get_topic(&stream_id, &topic_id)
                            .await?
                            .context("Iggy topic disappeared")?;
                        anyhow::ensure!(
                            details.partitions_count == 1,
                            "existing Iggy topic must have exactly one partition"
                        );
                        let partition = details
                            .partitions
                            .iter()
                            .find(|p| p.id == 1)
                            .context("Iggy partition 1 missing")?;
                        let history_until = if partition.messages_count == 0 {
                            0
                        } else {
                            partition.current_offset.saturating_add(1)
                        };
                        let start_offset = if config.replay_from_start {
                            0
                        } else {
                            history_until
                        };
                        (start_offset, history_until, None, bootstrap)
                    }
                    _ => anyhow::bail!("iggy client not initialized"),
                };

            let (tx, rx) = mpsc::channel::<TransportEvent>(256);
            let mut known_neighbors: HashSet<EndpointId> = HashSet::new();
            for peer in initial_peers.into_iter().take(128) {
                if peer != id && known_neighbors.insert(peer) {
                    let _ = tx.send(TransportEvent::NeighborUp(peer)).await;
                }
            }

            let poll_stream = stream_name.clone();
            let poll_topic = topic_name.clone();
            let poll_mode = config.mode.clone();
            let poll_client = client.clone();
            let poll_closed = Arc::clone(&closed);
            let poll_interval = config.poll_interval;

            let poll_task = tokio::spawn(async move {
                loop {
                    if poll_closed.load(Ordering::SeqCst) {
                        break;
                    }

                    let batch: Vec<PolledMessage> = match (&poll_mode, &poll_client) {
                        (IggyBackendMode::Memory(broker), _) => {
                            // Also check for any newly joined members in the memory broker
                            let active = {
                                let streams = broker.streams.lock().await;
                                streams
                                    .get(&(poll_stream.clone(), poll_topic.clone()))
                                    .map(|l| l.active_members.clone())
                                    .unwrap_or_default()
                            };
                            let departed: Vec<_> =
                                known_neighbors.difference(&active).copied().collect();
                            for peer in departed {
                                known_neighbors.remove(&peer);
                                if tx.send(TransportEvent::NeighborDown(peer)).await.is_err() {
                                    return;
                                }
                            }
                            for peer in active {
                                if peer != id
                                    && known_neighbors.insert(peer)
                                    && tx.send(TransportEvent::NeighborUp(peer)).await.is_err()
                                {
                                    return;
                                }
                            }
                            broker
                                .poll_messages(
                                    &poll_stream,
                                    &poll_topic,
                                    next_offset,
                                    POLL_BATCH_COUNT as usize,
                                )
                                .await
                        }
                        (IggyBackendMode::Server { .. }, Some(c)) => {
                            let Ok(stream_id) = Identifier::named(&poll_stream) else {
                                break;
                            };
                            let Ok(topic_id) = Identifier::named(&poll_topic) else {
                                break;
                            };
                            let consumer = Consumer::default();
                            match c
                                .poll_messages(
                                    &stream_id,
                                    &topic_id,
                                    Some(1),
                                    &consumer,
                                    &PollingStrategy::offset(next_offset),
                                    POLL_BATCH_COUNT,
                                    false,
                                )
                                .await
                            {
                                Ok(polled) => polled.messages,
                                Err(e) => {
                                    warn!(error = %e, "iggy polling failed; resubscribing");
                                    return;
                                }
                            }
                        }
                        _ => break,
                    };

                    if !batch.is_empty() {
                        for msg in batch {
                            next_offset = msg.offset.saturating_add(1);
                            if let Some(sender_peer) = sender_from_headers(&msg.headers) {
                                if sender_peer == id {
                                    continue;
                                }
                                if msg.offset >= history_until
                                    && known_neighbors.insert(sender_peer)
                                    && tx
                                        .send(TransportEvent::NeighborUp(sender_peer))
                                        .await
                                        .is_err()
                                {
                                    return;
                                }
                            }
                            debug!(
                                stream = %poll_stream,
                                topic = %poll_topic,
                                offset = msg.offset,
                                "received message from Iggy stream"
                            );
                            let event = if msg.offset < history_until {
                                TransportEvent::Historical(msg.payload)
                            } else {
                                TransportEvent::Received(msg.payload)
                            };
                            if tx.send(event).await.is_err() {
                                return;
                            }
                        }
                        continue;
                    }

                    if let Some(notify) = &topic_notify {
                        tokio::select! {
                            _ = closed_notify.notified() => break,
                            _ = notify.notified() => {},
                            _ = tokio::time::sleep(poll_interval) => {},
                        }
                    } else {
                        tokio::select! {
                            _ = closed_notify.notified() => break,
                            _ = tokio::time::sleep(poll_interval) => {},
                        }
                    }
                }
            });

            let membership = match &config.mode {
                IggyBackendMode::Memory(broker) => {
                    Some((broker.clone(), stream_name.clone(), topic_name.clone(), id))
                }
                _ => None,
            };
            let sender = Arc::new(IggyRoomSender {
                id,
                stream_name,
                topic_name,
                mode: config.mode,
                client,
                closed,
            }) as Arc<dyn RoomSender>;

            let receiver = Box::new(IggyRoomReceiver {
                rx,
                poll_task,
                membership,
            }) as Box<dyn RoomReceiver>;
            Ok((sender, receiver))
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.closed.store(true, Ordering::SeqCst);
            self.closed_notify.notify_waiters();
            if let Some(client) = &self.client {
                let _ = client.disconnect().await;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{FileActivityEntry, FileChangeKind};
    use crate::node::{BuddiesNode, BuddiesNodeConfig};
    use crate::protocol::room_to_topic;
    use crate::transport::TransportBackend;

    #[tokio::test]
    async fn iggy_transport_attaches_metadata_headers_and_streams_between_agents() {
        let broker = IggyMemoryBroker::new();
        let cfg = IggyConfig::in_memory(broker.clone());

        let alice = BuddiesNode::new(BuddiesNodeConfig {
            presence: crate::resilience::PresenceConfig::default(),
            user_name: "alice".into(),
            agent_name: "claude".into(),
            data_dir: None,
            signer: None,
            transport: TransportBackend::Iggy(cfg.clone()),
        })
        .await
        .expect("create alice over iggy");

        let bob = BuddiesNode::new(BuddiesNodeConfig {
            presence: crate::resilience::PresenceConfig::default(),
            user_name: "bob".into(),
            agent_name: "codex".into(),
            data_dir: None,
            signer: None,
            transport: TransportBackend::Iggy(cfg.clone()),
        })
        .await
        .expect("create bob over iggy");

        alice.join_room("sprint-room", None).await.unwrap();
        bob.join_room("sprint-room", None).await.unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            while alice
                .room_manager
                .get_room_peers("sprint-room")
                .await
                .is_empty()
                || bob
                    .room_manager
                    .get_room_peers("sprint-room")
                    .await
                    .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("alice and bob discover each other via Iggy stream");

        let entry = FileActivityEntry {
            repo: "buddies".into(),
            branch: "feat/iggy-streaming".into(),
            worktree: Some("wt-alice".into()),
            agent: "claude".into(),
            task_id: Some("TASK-42".into()),
            sprint: Some("Sprint-2026-W44".into()),
            path: "src/protocol.rs".into(),
            kind: FileChangeKind::Changed,
            diff: "+pub sprint: Option<String>,\n".into(),
            content_hash: "deadbeef".into(),
            author: "alice".into(),
            timestamp: crate::room::now_unix(),
            touched_symbols: vec!["FileActivityEntry".into()],
            affected_files: vec!["src/room.rs".into(), "src/server.rs".into()],
            impact_truncated: false,
        };

        alice
            .room_manager
            .broadcast_to_room(
                "sprint-room",
                P2PMessage::new(P2PMessageBody::FileActivityV2 {
                    entry: entry.clone(),
                }),
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            while bob
                .storage
                .get_peer_file_activity(
                    "buddies",
                    "src/protocol.rs",
                    "alice",
                    crate::room::now_unix(),
                )
                .await
                .unwrap()
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("bob receives FileActivity Entry over Iggy stream");

        // Verify the raw Iggy PolledMessage in the stream contains all headers!
        let topic_name = topic_name_for(room_to_topic("sprint-room"));
        let polled = broker.poll_messages("buddies", &topic_name, 0, 50).await;
        let file_msg = polled
            .iter()
            .find(|m| {
                m.headers
                    .as_ref()
                    .and_then(|h| h.get(&HeaderKey::new("buddies.task_id").unwrap()))
                    .and_then(|v| v.as_str().ok())
                    == Some("TASK-42")
            })
            .expect("file_activity message recorded in Iggy stream");

        let headers = file_msg.headers.as_ref().unwrap();
        let get_hdr = |k: &str| {
            headers
                .get(&HeaderKey::new(k).unwrap())
                .and_then(|v| v.as_str().ok())
                .map(str::to_string)
        };
        assert_eq!(get_hdr("buddies.repo").as_deref(), Some("buddies"));
        assert_eq!(
            get_hdr("buddies.branch").as_deref(),
            Some("feat/iggy-streaming")
        );
        assert_eq!(get_hdr("buddies.worktree").as_deref(), Some("wt-alice"));
        assert_eq!(get_hdr("buddies.agent").as_deref(), Some("claude"));
        assert_eq!(get_hdr("buddies.task_id").as_deref(), Some("TASK-42"));
        assert_eq!(
            get_hdr("buddies.sprint").as_deref(),
            Some("Sprint-2026-W44")
        );
        assert_eq!(get_hdr("buddies.path").as_deref(), Some("src/protocol.rs"));
        assert_eq!(
            get_hdr("buddies.affected_files").as_deref(),
            Some("src/room.rs,src/server.rs")
        );
        assert_eq!(
            headers
                .get(&HeaderKey::new("buddies.affected_count").unwrap())
                .and_then(|v| v.as_uint32().ok()),
            Some(2)
        );

        alice.shutdown().await.unwrap();
        bob.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires BUDDIES_TEST_IGGY_URL pointing at a real Iggy server"]
    async fn real_server_publish_replay_and_live_offsets() {
        let connection_string =
            std::env::var("BUDDIES_TEST_IGGY_URL").expect("BUDDIES_TEST_IGGY_URL");
        let config = IggyConfig {
            stream_name: format!("buddies-test-{}", uuid::Uuid::new_v4()),
            mode: IggyBackendMode::Server { connection_string },
            ..Default::default()
        };
        let a = tokio::time::timeout(
            Duration::from_secs(15),
            IggyTransport::new(iroh::SecretKey::generate().public(), config.clone()),
        )
        .await
        .expect("server connect timeout")
        .unwrap();
        let b = IggyTransport::new(iroh::SecretKey::generate().public(), config.clone())
            .await
            .unwrap();
        let topic = room_to_topic("integration");
        let (sender, _a_receiver) = a.subscribe(topic, vec![]).await.unwrap();
        let payload = P2PMessage::new(P2PMessageBody::StatusUpdate {
            author: "alice".into(),
            text: "persisted".into(),
        })
        .to_bytes();
        sender.broadcast(payload.clone()).await.unwrap();
        let (_b_sender, mut receiver) = b.subscribe(topic, vec![]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(TransportEvent::Historical(bytes)) =
                    receiver.next_event().await.unwrap()
                {
                    assert_eq!(bytes, payload);
                    break;
                }
            }
        })
        .await
        .expect("history replay");
        let mut latest_config = config.clone();
        latest_config.replay_from_start = false;
        let latest = IggyTransport::new(iroh::SecretKey::generate().public(), latest_config)
            .await
            .unwrap();
        let (_, mut latest_receiver) = latest.subscribe(topic, vec![]).await.unwrap();
        let next = P2PMessage::new(P2PMessageBody::StatusUpdate {
            author: "alice".into(),
            text: "live".into(),
        })
        .to_bytes();
        sender.broadcast(next.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(TransportEvent::Received(bytes)) =
                    latest_receiver.next_event().await.unwrap()
                {
                    assert_eq!(bytes, next);
                    break;
                }
            }
        })
        .await
        .expect("latest starts after historical high-water mark");
        a.client
            .as_ref()
            .unwrap()
            .delete_stream(&Identifier::named(&config.stream_name).unwrap())
            .await
            .unwrap();
        a.shutdown().await.unwrap();
        b.shutdown().await.unwrap();
        latest.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn connection_errors_and_debug_do_not_leak_credentials() {
        let config = IggyConfig {
            mode: IggyBackendMode::Server {
                connection_string: "invalid://alice:super-secret@localhost".into(),
            },
            ..Default::default()
        };
        assert!(!format!("{config:?}").contains("super-secret"));
        let result = IggyTransport::new(iroh::SecretKey::generate().public(), config).await;
        let message = match result {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("invalid connection accepted"),
        };
        assert!(!message.contains("super-secret"));
    }
}
