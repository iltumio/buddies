use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use iroh::protocol::Router;
use iroh::{Endpoint, endpoint::presets};
use iroh_gossip::net::Gossip;

use crate::activity::DirtySet;
use crate::async_storage::AsyncStorage;
use crate::identity::LocalSigner;
use crate::room::RoomManager;
use crate::watcher::WatcherManager;

/// Maximum gossip frame size. Must comfortably exceed the largest message
/// we broadcast: a FileActivity diff capped at
/// [`crate::activity::MAX_DIFF_BYTES`] (64 KiB) plus the postcard envelope
/// and signature. iroh-gossip's default (4096 bytes) kills peer connections
/// on larger frames; 256 KiB gives ample headroom. All peers must use the
/// same limit (same wire-compat posture as the message format itself).
pub(crate) const GOSSIP_MAX_MESSAGE_SIZE: usize = 256 * 1024;

pub struct BuddiesNode {
    ticket_operation: tokio::sync::Mutex<()>,
    ticket_addresses: iroh::address_lookup::memory::MemoryLookup,
    room_tickets: tokio::sync::Mutex<std::collections::HashMap<String, crate::ticket::RoomTicket>>,
    pub local_membership: tokio::sync::Mutex<()>,
    pub clients: crate::status::Clients,
    pub shutdown_token: tokio_util::sync::CancellationToken,
    pub endpoint: Endpoint,
    pub router: Router,
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
}

impl BuddiesNode {
    pub async fn new(config: BuddiesNodeConfig) -> Result<Self> {
        let storage = Arc::new(AsyncStorage::open(config.data_dir).await?);
        let ticket_addresses = iroh::address_lookup::memory::MemoryLookup::new();
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(storage.endpoint_secret().await?)
            .address_lookup(ticket_addresses.clone())
            .bind()
            .await?;

        let gossip = Gossip::builder()
            .max_message_size(GOSSIP_MAX_MESSAGE_SIZE)
            .spawn(endpoint.clone());

        let router = Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .spawn();

        let dirty = Arc::new(DirtySet::new());
        let author = config.user_name.clone();

        let room_manager = RoomManager::new(
            gossip,
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
            ticket_addresses,
            room_tickets: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            local_membership: tokio::sync::Mutex::new(()),
            clients: crate::status::Clients::default(),
            shutdown_token: tokio_util::sync::CancellationToken::new(),
            endpoint,
            router,
            room_manager,
            storage,
            watcher_manager,
        })
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
                ticket
                    .endpoints
                    .retain(|addr| addr.id != self.endpoint.id());
                ticket.endpoints.push(self.endpoint.addr());
                ticket
            }
        };
        if let Some(imported) = imported {
            for addr in imported.endpoints {
                if addr.id == self.endpoint.id() {
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
            .filter(|addr| addr.id != self.endpoint.id())
            .map(|addr| {
                self.ticket_addresses.add_endpoint_info(addr.clone());
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
        self.router.shutdown().await?;
        Ok(())
    }
}
