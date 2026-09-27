//! Read-only live state. Never opens the on-disk database.
pub use crate::local::Clients;
use crate::node::BuddiesNode;
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub node_id: String,
    pub signer: Option<String>,
    pub clients: Vec<Client>,
    pub rooms: Vec<Room>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Client {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub rooms: Vec<String>,
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
    #[serde(default = "remote_scope")]
    pub scope: String,
    pub name: String,
    pub agent: String,
    pub status: Option<String>,
    #[serde(default)]
    pub presence: crate::resilience::Presence,
    #[serde(default)]
    pub last_seen_secs: u64,
}

fn remote_scope() -> String {
    "remote".into()
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
                scope: "remote".into(),
                presence: p.presence(node.room_manager.presence),
                last_seen_secs: p.last_seen.elapsed().as_secs(),
                name: p.name,
                agent: p.agent,
                status: p.last_status,
            })
            .collect();
        peers.extend(node.clients.agents(&name, None));
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
