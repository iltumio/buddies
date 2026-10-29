use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::activity::FileActivityEntry;
use crate::memory::{MemoryEntry, SearchFilters};
use crate::negotiation::{NegotiationProposal, NegotiationResponse};
use crate::skill::{SkillEntry, SkillSearchFilters, SkillSearchResult};

pub type TopicId = iroh_gossip::proto::TopicId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct P2PMessage {
    pub nonce: [u8; 16],
    /// Unix timestamp set by the sender; signed messages outside the
    /// freshness window are dropped by receivers (replay protection).
    pub sent_at: u64,
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

    /// Stable voter key: comments and casing must not create extra votes.
    pub fn voting_label(&self) -> Option<String> {
        match self {
            Self::Gpg { key_id }
                if matches!(key_id.len(), 40 | 64)
                    && key_id.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                Some(format!("gpg:{}", key_id.to_ascii_uppercase()))
            }
            Self::Ssh { public_key } if !public_key.contains(['\n', '\r', '\0']) => {
                let mut parts = public_key.split_whitespace();
                let algorithm = parts.next()?;
                let key = parts.next()?;
                Some(format!("ssh:{algorithm} {key}"))
            }
            _ => None,
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
    SkillPublished {
        entry: SkillEntry,
    },
    SkillSearchRequest {
        request_id: Uuid,
        query: String,
        filters: SkillSearchFilters,
    },
    SkillSearchResponse {
        request_id: Uuid,
        results: Vec<SkillSearchResult>,
        peer_name: String,
    },
    SkillVoteCast {
        skill_hash: String,
        voter: String,
        score: i8,
    },
    FileActivity {
        #[serde(with = "legacy_activity")]
        entry: FileActivityEntry,
    },
    NegotiationProposed {
        proposal: NegotiationProposal,
    },
    NegotiationResponded {
        room: String,
        proposal_id: Uuid,
        response: NegotiationResponse,
    },
    /// Version 2 preserves the old variant's positional layout for older peers.
    FileActivityV2 {
        entry: FileActivityEntry,
    },
}

mod legacy_activity {
    use super::*;
    #[derive(Serialize, Deserialize)]
    struct Legacy {
        repo: String,
        branch: String,
        path: String,
        kind: crate::activity::FileChangeKind,
        diff: String,
        content_hash: String,
        author: String,
        timestamp: u64,
    }
    pub fn serialize<S: serde::Serializer>(
        entry: &FileActivityEntry,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        Legacy {
            repo: entry.repo.clone(),
            branch: entry.branch.clone(),
            path: entry.path.clone(),
            kind: entry.kind,
            diff: entry.diff.clone(),
            content_hash: entry.content_hash.clone(),
            author: entry.author.clone(),
            timestamp: entry.timestamp,
        }
        .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<FileActivityEntry, D::Error> {
        let old = Legacy::deserialize(deserializer)?;
        Ok(FileActivityEntry {
            repo: old.repo,
            branch: old.branch,
            path: old.path,
            kind: old.kind,
            diff: old.diff,
            content_hash: old.content_hash,
            author: old.author,
            timestamp: old.timestamp,
            worktree: None,
            agent: String::new(),
            task_id: None,
            sprint: None,
            touched_symbols: Vec::new(),
            affected_files: Vec::new(),
            impact_truncated: false,
        })
    }
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
            sent_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            body,
            signed_by: None,
            signature: None,
        }
    }

    pub fn signing_payload(&self) -> Bytes {
        postcard::to_allocvec(&(self.nonce, self.sent_at, &self.body))
            .expect("P2PMessage signing serialization is infallible")
            .into()
    }

    pub fn to_bytes(&self) -> Bytes {
        postcard::to_allocvec(self)
            .expect("P2PMessage serialization is infallible")
            .into()
    }

    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            bytes.len() <= crate::transport::GOSSIP_MAX_MESSAGE_SIZE,
            "P2P frame exceeds size limit"
        );
        Ok(postcard::from_bytes(bytes)?)
    }
}

pub fn room_to_topic(room_name: &str) -> TopicId {
    let mut hasher = Sha256::new();
    hasher.update(b"buddies:room:");
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
    #[test]
    fn voter_labels_do_not_allow_comment_or_case_aliases() {
        let a = SignerIdentity::Ssh {
            public_key: "ssh-ed25519 AAAA alice".into(),
        };
        let b = SignerIdentity::Ssh {
            public_key: "ssh-ed25519 AAAA bob".into(),
        };
        assert_eq!(a.voting_label(), b.voting_label());
        assert_eq!(
            SignerIdentity::Gpg {
                key_id: "abcd".repeat(10)
            }
            .voting_label(),
            SignerIdentity::Gpg {
                key_id: "ABCD".repeat(10)
            }
            .voting_label()
        );
        assert!(
            SignerIdentity::Gpg {
                key_id: "ABCD1234".into()
            }
            .voting_label()
            .is_none()
        );
    }

    #[test]
    fn legacy_activity_layout_and_signing_payload_are_frozen() {
        use super::*;
        #[derive(Serialize)]
        struct OldActivity {
            repo: String,
            branch: String,
            path: String,
            kind: crate::activity::FileChangeKind,
            diff: String,
            content_hash: String,
            author: String,
            timestamp: u64,
        }
        let old = OldActivity {
            repo: "repo".into(),
            branch: "main".into(),
            path: "src/lib.rs".into(),
            kind: crate::activity::FileChangeKind::Changed,
            diff: "+x".into(),
            content_hash: "abc".into(),
            author: "alice".into(),
            timestamp: 1000,
        };
        // Old enum index 13, followed by its exact original field order.
        let bytes = postcard::to_allocvec(&(
            [7u8; 16],
            1000u64,
            13u8,
            &old,
            Option::<SignerIdentity>::None,
            Option::<Vec<u8>>::None,
        ))
        .unwrap();
        let decoded = P2PMessage::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.to_bytes().as_ref(), bytes);
        assert_eq!(
            decoded.signing_payload().as_ref(),
            postcard::to_allocvec(&([7u8; 16], 1000u64, 13u8, &old)).unwrap()
        );
        let P2PMessageBody::FileActivity { mut entry } = decoded.body else {
            panic!("legacy variant");
        };
        assert_eq!(entry.path, "src/lib.rs");
        assert!(entry.agent.is_empty());
        entry.agent = "codex".into();
        entry.task_id = Some("TASK-1".into());
        entry.impact_truncated = true;
        let v2 = P2PMessage::new(P2PMessageBody::FileActivityV2 {
            entry: entry.clone(),
        });
        let P2PMessageBody::FileActivityV2 { entry: roundtrip } =
            P2PMessage::from_bytes(&v2.to_bytes()).unwrap().body
        else {
            panic!("v2");
        };
        assert_eq!(roundtrip, entry);
    }
}
