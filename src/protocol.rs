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
    pub signed_by: Option<SignerIdentity>,
    pub signature: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum SignerIdentity {
    Gpg { key_id: String },
    Ssh { public_key: String },
}

impl SignerIdentity {
    pub fn to_label(&self) -> String {
        match self {
            Self::Gpg { key_id } => format!("gpg:{key_id}"),
            Self::Ssh { public_key } => format!("ssh:{public_key}"),
        }
    }

    pub fn parse(label: &str) -> anyhow::Result<Self> {
        let (scheme, value) = label
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("identity must be 'gpg:<key>' or 'ssh:<pubkey>'"))?;
        let normalized = scheme.to_ascii_lowercase();
        if normalized == "gpg" {
            return Ok(Self::Gpg {
                key_id: value.to_string(),
            });
        }
        if normalized == "ssh" {
            return Ok(Self::Ssh {
                public_key: value.to_string(),
            });
        }
        anyhow::bail!("unsupported identity scheme '{scheme}'")
    }
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
            signed_by: None,
            signature: None,
        }
    }

    pub fn signing_payload(&self) -> Bytes {
        postcard::to_allocvec(&(self.nonce, &self.body))
            .expect("P2PMessage signing serialization is infallible")
            .into()
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

#[cfg(test)]
mod tests {
    use super::SignerIdentity;

    #[test]
    fn signer_identity_parse_and_label_roundtrip() {
        let gpg = SignerIdentity::parse("gpg:ABC123").expect("parse gpg identity");
        assert_eq!(
            gpg,
            SignerIdentity::Gpg {
                key_id: "ABC123".into()
            }
        );
        assert_eq!(gpg.to_label(), "gpg:ABC123");

        let ssh_key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMockKey user@host";
        let ssh = SignerIdentity::parse(&format!("ssh:{ssh_key}")).expect("parse ssh identity");
        assert_eq!(
            ssh,
            SignerIdentity::Ssh {
                public_key: ssh_key.into()
            }
        );
        assert_eq!(ssh.to_label(), format!("ssh:{ssh_key}"));
    }

    #[test]
    fn signer_identity_parse_rejects_unknown_scheme() {
        let err = SignerIdentity::parse("x509:foo").expect_err("must reject unknown scheme");
        assert!(err.to_string().contains("unsupported identity scheme"));
    }
}
