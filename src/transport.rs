use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use bytes::Bytes;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::presets};
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use iroh_gossip::net::Gossip;
use tokio::sync::{Mutex, Notify, broadcast};

use crate::async_storage::AsyncStorage;
use crate::iggy_transport::{IggyConfig, IggyTransport};
use crate::protocol::TopicId;

/// Maximum gossip frame size. Must comfortably exceed the largest message
/// we broadcast: a FileActivity diff capped at
/// [`crate::activity::MAX_DIFF_BYTES`] (64 KiB) plus the postcard envelope
/// and signature. iroh-gossip's default (4096 bytes) kills peer connections
/// on larger frames; 256 KiB gives ample headroom. All peers must use the
/// same limit (same wire-compat posture as the message format itself).
pub const GOSSIP_MAX_MESSAGE_SIZE: usize = 256 * 1024;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Active room subscription handle split into sender and receiver halves.
pub type RoomSubscription = (Arc<dyn RoomSender>, Box<dyn RoomReceiver>);

/// Events emitted by a room transport subscription.
#[derive(Debug, Clone)]
pub enum TransportEvent {
    /// A serialized `P2PMessage` payload arrived on the room topic.
    Received(Bytes),
    /// A record predating this subscription; only authenticated durable events may replay.
    Historical(Bytes),
    /// A direct transport neighbor became reachable on this topic.
    NeighborUp(EndpointId),
    /// A direct transport neighbor disconnected on this topic.
    NeighborDown(EndpointId),
    /// The underlying event stream fell behind and should be resynchronized.
    Lagged,
}

/// Sender half of an active room subscription.
pub trait RoomSender: Send + Sync {
    /// Broadcast a serialized message to all peers subscribed to the room.
    fn broadcast(&self, payload: Bytes) -> BoxFuture<'_, Result<()>>;

    /// Request that the transport dial/join the given bootstrap peer IDs.
    fn join_peers(&self, peers: Vec<EndpointId>) -> BoxFuture<'_, Result<()>>;
}

/// Receiver half of an active room subscription.
pub trait RoomReceiver: Send {
    /// Wait for the next transport event on the room topic (`Ok(None)` when closed).
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<TransportEvent>>>;
}

/// Pluggable communication transport for room pub/sub and peer discovery.
///
/// Implementations include [`IrohTransport`] (P2P over `iroh` + `iroh-gossip`),
/// [`IggyTransport`] (persistent message streaming over `iggy-rs`),
/// and [`LocalTransport`] (in-memory pub/sub without any network sockets).
pub trait RoomTransport: Send + Sync {
    /// Stable node/endpoint identifier for this transport instance.
    fn node_id(&self) -> EndpointId;

    /// Reachable endpoint address for room tickets.
    fn endpoint_addr(&self) -> EndpointAddr;

    /// Register a peer's endpoint address discovered via an imported room ticket.
    fn register_peer_addr(&self, addr: EndpointAddr);

    /// Subscribe to a room topic and connect to `bootstrap` peers.
    fn subscribe(
        &self,
        topic: TopicId,
        bootstrap: Vec<EndpointId>,
    ) -> BoxFuture<'_, Result<RoomSubscription>>;

    /// Shut down the underlying transport resources.
    fn shutdown(&self) -> BoxFuture<'_, Result<()>>;
}

/// Transport selection for [`crate::node::BuddiesNode`].
#[derive(Clone, Default)]
pub enum TransportBackend {
    /// P2P QUIC + gossip transport powered by Iroh (default).
    #[default]
    Iroh,
    /// Persistent message streaming powered by Apache Iggy (`iggy-rs`).
    Iggy(IggyConfig),
    /// In-memory transport (no UDP/QUIC sockets; shares a [`LocalHub`] across in-process nodes).
    Local(LocalHub),
    /// Custom user-provided [`RoomTransport`] implementation.
    #[allow(dead_code)]
    Custom(Arc<dyn RoomTransport>),
}

impl std::fmt::Debug for TransportBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Iroh => f.write_str("TransportBackend::Iroh"),
            Self::Iggy(_) => f.write_str("TransportBackend::Iggy"),
            Self::Local(_) => f.write_str("TransportBackend::Local"),
            Self::Custom(_) => f.write_str("TransportBackend::Custom"),
        }
    }
}

impl TransportBackend {
    /// Resolve the transport backend from `BUDDIES_NET_TRANSPORT` (`"iroh"`, `"iggy"`, or `"local"`).
    pub fn from_env() -> Result<Self> {
        match std::env::var("BUDDIES_NET_TRANSPORT") {
            Ok(val) => match val.trim().to_ascii_lowercase().as_str() {
                "" | "iroh" | "p2p" => Ok(Self::Iroh),
                "iggy" | "iggy-rs" | "stream" => Ok(Self::Iggy(IggyConfig::from_env()?)),
                "local" | "memory" | "in-memory" => Ok(Self::Local(LocalHub::global())),
                other => anyhow::bail!(
                    "unsupported BUDDIES_NET_TRANSPORT '{other}' (expected 'iroh', 'iggy', or 'local')"
                ),
            },
            Err(_) => Ok(Self::Iroh),
        }
    }

    pub async fn build(&self, storage: &AsyncStorage) -> Result<Arc<dyn RoomTransport>> {
        match self {
            Self::Iroh => {
                let secret = storage.endpoint_secret().await?;
                Ok(Arc::new(IrohTransport::bind(secret).await?))
            }
            Self::Iggy(config) => {
                let secret = storage.endpoint_secret().await?;
                Ok(Arc::new(
                    IggyTransport::new(secret.public(), config.clone()).await?,
                ))
            }
            Self::Local(hub) => {
                let secret = storage.endpoint_secret().await?;
                Ok(Arc::new(LocalTransport::new(secret.public(), hub.clone())))
            }
            Self::Custom(transport) => Ok(Arc::clone(transport)),
        }
    }
}

// ============================================================================
// Iroh P2P Transport (`iroh` + `iroh-gossip`)
// ============================================================================

pub struct IrohTransport {
    ticket_addresses: iroh::address_lookup::memory::MemoryLookup,
    endpoint: Endpoint,
    gossip: Gossip,
    router: Router,
}

impl IrohTransport {
    pub async fn bind(secret_key: iroh::SecretKey) -> Result<Self> {
        let ticket_addresses = iroh::address_lookup::memory::MemoryLookup::new();
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .address_lookup(ticket_addresses.clone())
            .bind()
            .await?;

        let gossip = Gossip::builder()
            .max_message_size(GOSSIP_MAX_MESSAGE_SIZE)
            .spawn(endpoint.clone());

        let router = Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .spawn();

        Ok(Self {
            ticket_addresses,
            endpoint,
            gossip,
            router,
        })
    }

    /// Construct an [`IrohTransport`] from pre-built Iroh components.
    #[cfg(test)]
    pub fn from_parts(
        endpoint: Endpoint,
        gossip: Gossip,
        router: Router,
        ticket_addresses: iroh::address_lookup::memory::MemoryLookup,
    ) -> Self {
        Self {
            ticket_addresses,
            endpoint,
            gossip,
            router,
        }
    }
}

struct IrohRoomSender(GossipSender);

impl RoomSender for IrohRoomSender {
    fn broadcast(&self, payload: Bytes) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.0.broadcast(payload).await?;
            Ok(())
        })
    }

    fn join_peers(&self, peers: Vec<EndpointId>) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.0.join_peers(peers).await?;
            Ok(())
        })
    }
}

struct IrohRoomReceiver(GossipReceiver);

impl RoomReceiver for IrohRoomReceiver {
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<TransportEvent>>> {
        Box::pin(async move {
            use n0_future::TryStreamExt;
            let event = self.0.try_next().await?;
            Ok(event.map(|ev| match ev {
                Event::Received(msg) => TransportEvent::Received(msg.content),
                Event::NeighborUp(id) => TransportEvent::NeighborUp(id),
                Event::NeighborDown(id) => TransportEvent::NeighborDown(id),
                Event::Lagged => TransportEvent::Lagged,
            }))
        })
    }
}

impl RoomTransport for IrohTransport {
    fn node_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    fn endpoint_addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    fn register_peer_addr(&self, addr: EndpointAddr) {
        self.ticket_addresses.add_endpoint_info(addr);
    }

    fn subscribe(
        &self,
        topic: TopicId,
        bootstrap: Vec<EndpointId>,
    ) -> BoxFuture<'_, Result<RoomSubscription>> {
        Box::pin(async move {
            let sub = self.gossip.subscribe(topic, bootstrap).await?;
            let (sender, receiver) = sub.split();
            Ok((
                Arc::new(IrohRoomSender(sender)) as Arc<dyn RoomSender>,
                Box::new(IrohRoomReceiver(receiver)) as Box<dyn RoomReceiver>,
            ))
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let _ = self.gossip.shutdown().await;
            self.router.shutdown().await?;
            Ok(())
        })
    }
}

// ============================================================================
// Local / In-Memory Transport (zero network stack)
// ============================================================================

#[derive(Clone, Debug)]
enum HubPacket {
    Message { from: EndpointId, payload: Bytes },
    PeerJoined { id: EndpointId },
    PeerLeft { id: EndpointId },
}

struct TopicBus {
    tx: broadcast::Sender<HubPacket>,
    members: HashSet<EndpointId>,
}

/// Shared in-memory bus connecting [`LocalTransport`] instances.
#[derive(Clone, Default)]
pub struct LocalHub {
    topics: Arc<Mutex<HashMap<TopicId, TopicBus>>>,
}

impl std::fmt::Debug for LocalHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalHub").finish()
    }
}

impl LocalHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process-wide default [`LocalHub`].
    pub fn global() -> Self {
        static GLOBAL: std::sync::OnceLock<LocalHub> = std::sync::OnceLock::new();
        GLOBAL.get_or_init(LocalHub::new).clone()
    }
}

/// In-memory [`RoomTransport`] implementation that requires no network sockets.
pub struct LocalTransport {
    id: EndpointId,
    hub: LocalHub,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
    known_addrs: std::sync::Mutex<HashMap<EndpointId, EndpointAddr>>,
}

impl LocalTransport {
    pub fn new(id: EndpointId, hub: LocalHub) -> Self {
        Self {
            id,
            hub,
            closed: Arc::new(AtomicBool::new(false)),
            closed_notify: Arc::new(Notify::new()),
            known_addrs: std::sync::Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub fn ephemeral(hub: LocalHub) -> Self {
        let secret = iroh::SecretKey::generate();
        Self::new(secret.public(), hub)
    }
}

struct LocalRoomSender {
    id: EndpointId,
    tx: broadcast::Sender<HubPacket>,
    closed: Arc<AtomicBool>,
}

impl RoomSender for LocalRoomSender {
    fn broadcast(&self, payload: Bytes) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            anyhow::ensure!(
                !self.closed.load(Ordering::SeqCst),
                "local transport closed"
            );
            let _ = self.tx.send(HubPacket::Message {
                from: self.id,
                payload,
            });
            Ok(())
        })
    }

    fn join_peers(&self, _peers: Vec<EndpointId>) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            anyhow::ensure!(
                !self.closed.load(Ordering::SeqCst),
                "local transport closed"
            );
            Ok(())
        })
    }
}

struct LocalRoomReceiver {
    id: EndpointId,
    topic: TopicId,
    hub: LocalHub,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
    pending_initial: Vec<TransportEvent>,
    rx: broadcast::Receiver<HubPacket>,
}

impl Drop for LocalRoomReceiver {
    fn drop(&mut self) {
        let id = self.id;
        let topic = self.topic;
        let hub = self.hub.clone();
        if let Ok(mut guard) = hub.topics.try_lock() {
            if let Some(bus) = guard.get_mut(&topic) {
                bus.members.remove(&id);
                let _ = bus.tx.send(HubPacket::PeerLeft { id });
            }
        } else {
            tokio::spawn(async move {
                let mut guard = hub.topics.lock().await;
                if let Some(bus) = guard.get_mut(&topic) {
                    bus.members.remove(&id);
                    let _ = bus.tx.send(HubPacket::PeerLeft { id });
                }
            });
        }
    }
}

impl RoomReceiver for LocalRoomReceiver {
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<TransportEvent>>> {
        Box::pin(async move {
            if self.closed.load(Ordering::SeqCst) {
                return Ok(None);
            }
            if let Some(ev) = self.pending_initial.pop() {
                return Ok(Some(ev));
            }
            loop {
                let notified = self.closed_notify.notified();
                if self.closed.load(Ordering::SeqCst) {
                    return Ok(None);
                }
                tokio::select! {
                    _ = notified => return Ok(None),
                    recv = self.rx.recv() => match recv {
                        Ok(HubPacket::Message { from, payload }) => {
                            if from != self.id {
                                return Ok(Some(TransportEvent::Received(payload)));
                            }
                        }
                        Ok(HubPacket::PeerJoined { id }) => {
                            if id != self.id {
                                return Ok(Some(TransportEvent::NeighborUp(id)));
                            }
                        }
                        Ok(HubPacket::PeerLeft { id }) => {
                            if id != self.id {
                                return Ok(Some(TransportEvent::NeighborDown(id)));
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            return Ok(Some(TransportEvent::Lagged));
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Ok(None);
                        }
                    }
                }
            }
        })
    }
}

impl RoomTransport for LocalTransport {
    fn node_id(&self) -> EndpointId {
        self.id
    }

    fn endpoint_addr(&self) -> EndpointAddr {
        EndpointAddr::new(self.id)
    }

    fn register_peer_addr(&self, addr: EndpointAddr) {
        if let Ok(mut map) = self.known_addrs.lock() {
            map.insert(addr.id, addr);
        }
    }

    fn subscribe(
        &self,
        topic: TopicId,
        _bootstrap: Vec<EndpointId>,
    ) -> BoxFuture<'_, Result<RoomSubscription>> {
        Box::pin(async move {
            anyhow::ensure!(
                !self.closed.load(Ordering::SeqCst),
                "local transport closed"
            );
            let mut topics = self.hub.topics.lock().await;
            let bus = topics.entry(topic).or_insert_with(|| TopicBus {
                tx: broadcast::channel(512).0,
                members: HashSet::new(),
            });
            let rx = bus.tx.subscribe();
            let pending_initial: Vec<TransportEvent> = bus
                .members
                .iter()
                .copied()
                .filter(|&peer| peer != self.id)
                .map(TransportEvent::NeighborUp)
                .collect();
            bus.members.insert(self.id);
            let _ = bus.tx.send(HubPacket::PeerJoined { id: self.id });
            let sender = Arc::new(LocalRoomSender {
                id: self.id,
                tx: bus.tx.clone(),
                closed: Arc::clone(&self.closed),
            }) as Arc<dyn RoomSender>;
            let receiver = Box::new(LocalRoomReceiver {
                id: self.id,
                topic,
                hub: self.hub.clone(),
                closed: Arc::clone(&self.closed),
                closed_notify: Arc::clone(&self.closed_notify),
                pending_initial,
                rx,
            }) as Box<dyn RoomReceiver>;
            Ok((sender, receiver))
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.closed.store(true, Ordering::SeqCst);
            self.closed_notify.notify_waiters();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryEntry, MemoryKind, SearchFilters};
    use crate::node::{BuddiesNode, BuddiesNodeConfig};
    use crate::protocol::{P2PMessage, P2PMessageBody, TaskResult};
    use std::time::Duration;
    use uuid::Uuid;

    #[tokio::test]
    async fn local_transport_replicates_memories_and_delegates_tasks_without_network() {
        let hub = LocalHub::new();

        let alice = BuddiesNode::new(BuddiesNodeConfig {
            presence: crate::resilience::PresenceConfig::default(),
            user_name: "alice".into(),
            agent_name: "claude".into(),
            data_dir: None,
            signer: None,
            transport: TransportBackend::Local(hub.clone()),
        })
        .await
        .expect("create alice");

        let bob_custom: Arc<dyn RoomTransport> = Arc::new(LocalTransport::ephemeral(hub.clone()));
        let bob = BuddiesNode::new(BuddiesNodeConfig {
            presence: crate::resilience::PresenceConfig::default(),
            user_name: "bob".into(),
            agent_name: "codex".into(),
            data_dir: None,
            signer: None,
            transport: TransportBackend::Custom(bob_custom),
        })
        .await
        .expect("create bob");

        let ticket = alice.join_room("team-room", None).await.unwrap();
        bob.join_room("team-room", Some(&ticket.to_string()))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            while alice
                .room_manager
                .get_room_peers("team-room")
                .await
                .is_empty()
                || bob
                    .room_manager
                    .get_room_peers("team-room")
                    .await
                    .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("alice and bob discover each other over LocalTransport");

        // Broadcast memory from Alice and verify Bob receives it + distributed search works
        let entry = MemoryEntry {
            id: Uuid::new_v4(),
            author: alice.room_manager.peer_id().to_owned(),
            timestamp: crate::room::now_unix(),
            room: "team-room".into(),
            kind: MemoryKind::Decision,
            title: "Pluggable transport".into(),
            content: "Use RoomTransport trait for Iroh and Local backends".into(),
            tags: vec!["architecture".into()],
            references: vec![],
        };
        alice.storage.store(&entry).await.unwrap();
        alice
            .room_manager
            .broadcast_to_room(
                "team-room",
                P2PMessage::new(P2PMessageBody::MemoryCreated {
                    entry: entry.clone(),
                }),
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            while bob
                .storage
                .list(
                    &SearchFilters {
                        room: Some("team-room".into()),
                        ..Default::default()
                    },
                    10,
                )
                .await
                .unwrap()
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("bob receives replicated memory");

        // Delegate a task from Alice to Bob over LocalTransport
        let bob_rm = Arc::clone(&bob.room_manager);
        let worker = tokio::spawn(async move {
            let task = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let tasks = bob_rm.poll_tasks(Some("team-room")).await;
                    if let Some(task) = tasks.into_iter().next() {
                        break task;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("bob receives delegated task");

            bob_rm
                .submit_task_result(
                    &task,
                    TaskResult::Success {
                        output: format!("handled: {}", task.description),
                    },
                )
                .await
                .unwrap();
        });

        let outcome = alice
            .room_manager
            .delegate_task("team-room", "verify transport abstraction", 5)
            .await
            .unwrap();
        worker.await.unwrap();
        assert!(matches!(
            outcome,
            TaskResult::Success { ref output } if output == "handled: verify transport abstraction"
        ));

        alice.shutdown().await.unwrap();
        bob.shutdown().await.unwrap();
    }
}
