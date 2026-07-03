use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use iroh::Endpoint;
use iroh::protocol::Router;
use iroh_gossip::net::Gossip;

use crate::activity::DirtySet;
use crate::identity::LocalSigner;
use crate::room::RoomManager;
use crate::storage::Storage;
use crate::watcher::WatcherManager;

/// Maximum gossip frame size. Must comfortably exceed the largest message
/// we broadcast: a FileActivity diff capped at
/// [`crate::activity::MAX_DIFF_BYTES`] (64 KiB) plus the postcard envelope
/// and signature. iroh-gossip's default (4096 bytes) kills peer connections
/// on larger frames; 256 KiB gives ample headroom. All peers must use the
/// same limit (same wire-compat posture as the message format itself).
const GOSSIP_MAX_MESSAGE_SIZE: usize = 256 * 1024;

pub struct BuddiesNode {
    pub endpoint: Endpoint,
    pub router: Router,
    pub room_manager: Arc<RoomManager>,
    pub storage: Arc<Storage>,
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
        let endpoint = Endpoint::builder().bind().await?;

        let gossip = Gossip::builder()
            .max_message_size(GOSSIP_MAX_MESSAGE_SIZE)
            .spawn(endpoint.clone());

        let router = Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .spawn();

        let storage = if let Some(ref dir) = config.data_dir {
            std::fs::create_dir_all(dir)?;
            Arc::new(Storage::open(&dir.join("buddies.redb"))?)
        } else {
            Arc::new(Storage::in_memory()?)
        };

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
    ) -> tokio::sync::broadcast::Receiver<crate::activity::FileActivityEntry> {
        self.room_manager.subscribe_conflict_events()
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.router.shutdown().await?;
        Ok(())
    }
}
