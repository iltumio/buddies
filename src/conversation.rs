//! Durable, addressed conversations on a shared buddies service.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

pub const RETENTION_SECS: u64 = 7 * 86_400;
const MAX_MESSAGES: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub in_reply_to: Option<Uuid>,
    pub room: String,
    pub from: String,
    pub to: String,
    pub kind: String,
    pub content: String,
    pub timestamp: u64,
    pub state: String,
    pub detail: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Conversations {
    participants: BTreeSet<(String, String)>,
    messages: Vec<Message>,
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
        "participant name must be 1..64 ASCII letters, digits, dots, underscores or hyphens"
    );
    Ok(())
}

impl Conversations {
    pub fn register(&mut self, room: &str, name: &str) -> Result<()> {
        validate_name(name)?;
        ensure!(
            self.participants.contains(&(room.into(), name.into()))
                || self.participants.len() < 512,
            "participant registry is full"
        );
        self.participants.insert((room.into(), name.into()));
        Ok(())
    }

    pub fn send(&mut self, mut message: Message, now: u64) -> Result<Message> {
        self.messages
            .retain(|m| now.saturating_sub(m.timestamp) < RETENTION_SECS);
        ensure!(
            self.messages.len() < MAX_MESSAGES,
            "message history is full; retention is seven days"
        );
        ensure!(
            self.participants
                .contains(&(message.room.clone(), message.to.clone())),
            "recipient must register in this room first (it may currently be offline)"
        );
        ensure!(
            self.participants
                .contains(&(message.room.clone(), message.from.clone())),
            "sender must register in this room first"
        );
        ensure!(
            message.from != message.to,
            "cannot send a message to yourself"
        );
        ensure!(
            !message.content.trim().is_empty() && message.content.len() <= 16 * 1024,
            "message must be 1..16384 bytes"
        );
        ensure!(
            ["question", "answer", "request", "update"].contains(&message.kind.as_str()),
            "kind must be question, answer, request or update"
        );
        if let Some(parent_id) = message.in_reply_to {
            let parent = self
                .messages
                .iter()
                .find(|m| m.id == parent_id)
                .ok_or_else(|| anyhow::anyhow!("reply refers to an unknown or expired message"))?;
            ensure!(
                parent.room == message.room
                    && ((parent.to == message.from && parent.from == message.to)
                        || (parent.from == message.from && parent.to == message.to)),
                "follow-up must retain both participants and the room"
            );
            message.conversation_id = parent.conversation_id;
        } else {
            // A new conversation always gets a server-generated identifier.
            message.conversation_id = message.id;
        }
        self.messages.push(message.clone());
        Ok(message)
    }

    pub fn inbox(&self, room: &str, name: &str, now: u64) -> Vec<Message> {
        self.messages
            .iter()
            .filter(|m| {
                m.room == room
                    && m.to == name
                    && !["handled", "failed"].contains(&m.state.as_str())
                    && now.saturating_sub(m.timestamp) < RETENTION_SECS
            })
            .cloned()
            .collect()
    }

    pub fn history(&self, room: &str, name: &str, conversation: Uuid, now: u64) -> Vec<Message> {
        self.messages
            .iter()
            .filter(|m| {
                m.room == room
                    && m.conversation_id == conversation
                    && (m.from == name || m.to == name)
                    && now.saturating_sub(m.timestamp) < RETENTION_SECS
            })
            .cloned()
            .collect()
    }

    pub fn acknowledge(
        &mut self,
        room: &str,
        name: &str,
        id: Uuid,
        state: &str,
        detail: Option<String>,
    ) -> Result<Message> {
        ensure!(
            ["read", "handled", "failed"].contains(&state),
            "state must be read, handled or failed"
        );
        ensure!(
            detail.as_ref().is_none_or(|d| d.len() <= 4096),
            "detail exceeds 4096 bytes"
        );
        let message = self
            .messages
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown message"))?;
        ensure!(
            message.room == room && message.to == name,
            "only the recipient can acknowledge this message"
        );
        ensure!(
            !["handled", "failed"].contains(&message.state.as_str()) || message.state == state,
            "a terminal acknowledgement cannot be changed"
        );
        message.state = state.into();
        message.detail = detail;
        Ok(message.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(room: &str, from: &str, to: &str) -> Message {
        Message {
            id: Uuid::new_v4(),
            conversation_id: Uuid::nil(),
            in_reply_to: None,
            room: room.into(),
            from: from.into(),
            to: to.into(),
            kind: "question".into(),
            content: "Which field?".into(),
            timestamp: 1,
            state: "pending".into(),
            detail: None,
        }
    }
    #[test]
    fn conversations_require_matching_parties_and_receipts_survive_serialization() {
        let mut q = Conversations::default();
        q.register("room", "alice").unwrap();
        q.register("room", "bob").unwrap();
        let a = q.send(message("room", "alice", "bob"), 1).unwrap();
        assert!(q.acknowledge("room", "alice", a.id, "read", None).is_err());
        assert!(q.acknowledge("other", "bob", a.id, "read", None).is_err());
        let mut reply = message("room", "bob", "alice");
        reply.in_reply_to = Some(a.id);
        let b = q.send(reply.clone(), 1).unwrap();
        assert_eq!(b.conversation_id, a.id);
        let mut update = message("room", "alice", "bob");
        update.kind = "update".into();
        update.in_reply_to = Some(a.id);
        assert_eq!(q.send(update, 1).unwrap().conversation_id, a.id);
        reply.room = "other".into();
        q.register("other", "alice").unwrap();
        q.register("other", "bob").unwrap();
        assert!(q.send(reply, 1).is_err());
        q.register("room", "outsider").unwrap();
        let mut forged = message("room", "outsider", "alice");
        forged.in_reply_to = Some(a.id);
        assert!(q.send(forged, 1).is_err());
        q.acknowledge("room", "bob", a.id, "handled", None).unwrap();
        assert!(q.acknowledge("room", "bob", a.id, "read", None).is_err());
        let restored: Conversations =
            serde_json::from_slice(&serde_json::to_vec(&q).unwrap()).unwrap();
        assert_eq!(restored.inbox("room", "bob", 1).len(), 1); // Only the unhandled update remains.
        assert_eq!(restored.inbox("room", "alice", 1).len(), 1);
        assert!(restored.history("room", "outsider", a.id, 1).is_empty());
    }
}
