use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use iroh::EndpointId;

use crate::activity::DirtySet;
use crate::async_storage::AsyncStorage;
use crate::identity::LocalSigner;
use crate::room::RoomManager;
use crate::transport::{RoomTransport, TransportBackend};
use crate::watcher::WatcherManager;

pub use crate::transport::GOSSIP_MAX_MESSAGE_SIZE;

pub struct BuddiesNode {
    ticket_operation: tokio::sync::Mutex<()>,
    room_tickets: tokio::sync::Mutex<std::collections::HashMap<String, crate::ticket::RoomTicket>>,
    pub local_membership: tokio::sync::Mutex<()>,
    pub clients: crate::status::Clients,
    pub shutdown_token: tokio_util::sync::CancellationToken,
    pub transport: Arc<dyn RoomTransport>,
    pub room_manager: Arc<RoomManager>,
    pub storage: Arc<AsyncStorage>,
    pub watcher_manager: Arc<WatcherManager>,
}

pub struct BuddiesNodeConfig {
    pub presence: crate::resilience::PresenceConfig,
    pub user_name: String,
    pub agent_name: String,
    pub data_dir: Option<PathBuf>,
    pub signer: Option<LocalSigner>,
    pub transport: TransportBackend,
}

impl BuddiesNode {
    pub async fn new(config: BuddiesNodeConfig) -> Result<Self> {
        let storage = Arc::new(AsyncStorage::open(config.data_dir).await?);
        let transport = config.transport.build(&storage).await?;

        let dirty = Arc::new(DirtySet::new());
        let author = config.user_name.clone();

        let room_manager = RoomManager::new(
            Arc::clone(&transport),
            config.user_name,
            config.agent_name,
            Arc::clone(&storage),
            config.signer,
            Arc::clone(&dirty),
            config.presence,
        );

        let watcher_manager = WatcherManager::new(Arc::clone(&room_manager), dirty, author);

        Ok(Self {
            ticket_operation: tokio::sync::Mutex::new(()),
            room_tickets: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            local_membership: tokio::sync::Mutex::new(()),
            clients: crate::status::Clients::default(),
            shutdown_token: tokio_util::sync::CancellationToken::new(),
            transport,
            room_manager,
            storage,
            watcher_manager,
        })
    }

    pub fn node_id(&self) -> EndpointId {
        self.transport.node_id()
    }

    /// The process owns room tickets; sessions only choose the room name.
    pub async fn join_room(
        &self,
        room: &str,
        supplied: Option<&str>,
    ) -> Result<crate::ticket::RoomTicket> {
        use crate::{protocol::room_to_topic, ticket::RoomTicket};
        let _operation = self.ticket_operation.lock().await;
        let topic = room_to_topic(room);
        let imported = supplied.map(str::parse::<RoomTicket>).transpose()?;
        if let Some(ticket) = &imported {
            anyhow::ensure!(ticket.room == room, "ticket belongs to a different room");
            anyhow::ensure!(ticket.topic == topic, "ticket topic does not match room");
            anyhow::ensure!(
                !ticket.endpoints.is_empty(),
                "external ticket has no endpoints"
            );
        }
        let node_id = self.transport.node_id();
        let node_addr = self.transport.endpoint_addr();
        let cached = self.room_tickets.lock().await.get(room).cloned();
        let mut ticket = match cached {
            Some(ticket) => ticket,
            None => {
                let mut ticket = self
                    .storage
                    .room_ticket(room)
                    .await?
                    .unwrap_or_else(|| RoomTicket::new(room.to_owned(), topic, vec![]));
                anyhow::ensure!(
                    ticket.room == room && ticket.topic == topic,
                    "invalid stored room ticket"
                );
                // The identity is stable; addresses may change when the process restarts.
                ticket.endpoints.retain(|addr| addr.id != node_id);
                ticket.endpoints.push(node_addr);
                ticket
            }
        };
        if let Some(imported) = imported {
            for addr in imported.endpoints {
                if addr.id == node_id {
                    continue;
                }
                ticket.endpoints.retain(|existing| existing.id != addr.id);
                ticket.endpoints.push(addr);
            }
        }
        anyhow::ensure!(
            ticket.endpoints.len() <= 128,
            "too many room endpoints (maximum 128)"
        );
        let bootstrap = ticket
            .endpoints
            .iter()
            .filter(|addr| addr.id != node_id)
            .map(|addr| {
                self.transport.register_peer_addr(addr.clone());
                addr.id
            })
            .collect();
        // Persist before joining: retries and new sessions must not lose an import.
        self.storage.save_room_ticket(&ticket).await?;
        self.room_tickets
            .lock()
            .await
            .insert(room.to_owned(), ticket.clone());
        self.room_manager.join_room(room, bootstrap).await?;
        Ok(ticket)
    }

    pub fn subscribe_task_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::room::PendingTask> {
        self.room_manager.subscribe_task_events()
    }

    pub fn subscribe_conflict_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::activity::ConflictEvent> {
        self.room_manager.subscribe_conflict_events()
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.shutdown_token.cancel();
        self.watcher_manager.shutdown().await;
        self.room_manager.shutdown().await;
        self.transport.shutdown().await?;
        Ok(())
    }
}
