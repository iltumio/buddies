use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use iroh::protocol::Router;
use iroh::Endpoint;
use iroh_gossip::net::Gossip;

use crate::identity::LocalSigner;
use crate::room::RoomManager;
use crate::storage::Storage;

pub struct BuddiesNode {
    pub endpoint: Endpoint,
    pub router: Router,
    pub room_manager: Arc<RoomManager>,
    pub storage: Arc<Storage>,
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

        let room_manager = RoomManager::new(
            gossip,
            config.user_name,
            config.agent_name,
            Arc::clone(&storage),
            config.signer,
        );

        Ok(Self {
            endpoint,
            router,
            room_manager,
            storage,
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.router.shutdown().await?;
        Ok(())
    }
}
