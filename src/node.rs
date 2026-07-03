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

        let gossip = Gossip::builder().spawn(endpoint.clone());

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
