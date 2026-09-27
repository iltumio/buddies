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
    pub clients: crate::status::Clients,
    pub shutdown_token: tokio_util::sync::CancellationToken,
    pub endpoint: Endpoint,
    pub router: Router,
    pub room_manager: Arc<RoomManager>,
    pub storage: Arc<AsyncStorage>,
    pub watcher_manager: Arc<WatcherManager>,
}

pub struct BuddiesNodeConfig {
    pub user_name: String,
    pub agent_name: String,
    pub data_dir: Option<PathBuf>,
    pub signer: Option<LocalSigner>,
}

impl BuddiesNode {
    pub async fn new(config: BuddiesNodeConfig) -> Result<Self> {
        let endpoint = Endpoint::builder(presets::N0).bind().await?;

        let gossip = Gossip::builder()
            .max_message_size(GOSSIP_MAX_MESSAGE_SIZE)
            .spawn(endpoint.clone());

        let router = Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .spawn();

        let storage = Arc::new(AsyncStorage::open(config.data_dir).await?);

        let dirty = Arc::new(DirtySet::new());
        let author = config.user_name.clone();

        let room_manager = RoomManager::new(
            gossip,
            config.user_name,
            config.agent_name,
            Arc::clone(&storage),
            config.signer,
            Arc::clone(&dirty),
        );

        let watcher_manager = WatcherManager::new(Arc::clone(&room_manager), dirty, author);

        Ok(Self {
            clients: crate::status::Clients::default(),
            shutdown_token: tokio_util::sync::CancellationToken::new(),
            endpoint,
            router,
            room_manager,
            storage,
            watcher_manager,
        })
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
