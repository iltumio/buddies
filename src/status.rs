//! Read-only live state. Never opens the on-disk database.
use std::sync::{Arc, Mutex};

use axum::{Json, extract::State};
use rmcp::{Peer, RoleServer};
use serde::{Deserialize, Serialize};

use crate::node::BuddiesNode;

#[derive(Default)]
pub struct Clients(Mutex<Vec<Peer<RoleServer>>>);

impl Clients {
    pub fn prune(&self) {
        self.0.lock().unwrap().retain(|p| !p.is_transport_closed());
    }

    pub fn register(&self, peer: Peer<RoleServer>) {
        let mut clients = self.0.lock().unwrap();
        clients.retain(|p| !p.is_transport_closed());
        clients.push(peer);
    }

    fn snapshot(&self) -> Vec<Client> {
        let mut clients = self.0.lock().unwrap();
        clients.retain(|p| !p.is_transport_closed());
        let mut result: Vec<_> = clients
            .iter()
            .filter_map(|p| p.peer_info())
            .map(|info| Client {
                name: info.client_info.name.clone(),
                version: info.client_info.version.clone(),
            })
            .collect();
        result.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
        result
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub node_id: String,
    pub signer: Option<String>,
    pub clients: Vec<Client>,
    pub rooms: Vec<Room>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Client {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    pub name: String,
    pub peers: Vec<Agent>,
    #[serde(default)]
    pub connection: crate::room::RoomHealth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub agent: String,
    pub status: Option<String>,
    #[serde(default)]
    pub presence: crate::resilience::Presence,
    #[serde(default)]
    pub last_seen_secs: u64,
}

pub async fn snapshot(State(node): State<Arc<BuddiesNode>>) -> Json<Snapshot> {
    let mut names = node.room_manager.list_rooms().await;
    names.sort();
    let mut rooms = Vec::with_capacity(names.len());
    for name in names {
        let mut peers: Vec<_> = node
            .room_manager
            .get_room_peers(&name)
            .await
            .into_values()
            .map(|p| Agent {
                presence: p.presence(node.room_manager.presence),
                last_seen_secs: p.last_seen.elapsed().as_secs(),
                name: p.name,
                agent: p.agent,
                status: p.last_status,
            })
            .collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        let connection = node
            .room_manager
            .room_health(&name)
            .await
            .unwrap_or_default();
        rooms.push(Room {
            name,
            peers,
            connection,
        });
    }
    Json(Snapshot {
        node_id: node.endpoint.id().to_string(),
        signer: node.room_manager.signer_identity_label(),
        clients: node.clients.snapshot(),
        rooms,
    })
}
