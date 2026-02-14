use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::memory::{MemoryEntry, SearchFilters};

pub type TopicId = iroh_gossip::proto::TopicId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct P2PMessage {
    pub nonce: [u8; 16],
    pub body: P2PMessageBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum P2PMessageBody {
    Join {
        name: String,
        agent: String,
    },
    Leave {
        name: String,
    },
    MemoryCreated {
        entry: MemoryEntry,
    },
    StatusUpdate {
        author: String,
        text: String,
    },
    SearchRequest {
        request_id: Uuid,
        query: String,
        filters: SearchFilters,
    },
    SearchResponse {
        request_id: Uuid,
        results: Vec<MemoryEntry>,
        peer_name: String,
    },
    TaskRequest {
        task_id: Uuid,
        source_peer: String,
        room: String,
        description: String,
        timeout_secs: u32,
        timestamp: u64,
    },
    TaskClaimed {
        task_id: Uuid,
        claimed_by: String,
    },
    TaskResponse {
        task_id: Uuid,
        result: TaskResult,
        completed_by: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskResult {
    Success { output: String },
    Error { message: String },
}

impl P2PMessage {
    pub fn new(body: P2PMessageBody) -> Self {
        Self {
            nonce: rand::random(),
            body,
        }
    }

    pub fn to_bytes(&self) -> Bytes {
        postcard::to_allocvec(self)
            .expect("P2PMessage serialization is infallible")
            .into()
    }

    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

pub fn room_to_topic(room_name: &str) -> TopicId {
    let mut hasher = Sha256::new();
    hasher.update(b"smemo:room:");
    hasher.update(room_name.as_bytes());
    let hash: [u8; 32] = hasher.finalize().into();
    TopicId::from_bytes(hash)
}
