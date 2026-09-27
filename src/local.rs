//! Participants in a shared node. IDs and room membership belong to MCP sessions.
use crate::{
    protocol::TaskResult,
    resilience::Presence,
    room::PendingTask,
    status::{Agent, Client},
};
use anyhow::{Result, anyhow, ensure};
use rmcp::{
    Peer, RoleServer,
    model::{CustomNotification, ServerNotification},
};
use std::{collections::HashMap, sync::Mutex, time::Duration};
use tokio::{sync::oneshot, time::Instant};
use uuid::Uuid;

struct Participant {
    peer: Peer<RoleServer>,
    rooms: HashMap<String, Option<String>>,
}
struct Task {
    request: PendingTask,
    requester: Uuid,
    target: Uuid,
    deadline: Instant,
    result: oneshot::Sender<TaskResult>,
    polled: bool,
}
#[derive(Default)]
struct State {
    participants: HashMap<Uuid, Participant>,
    tasks: HashMap<Uuid, Task>,
}
#[derive(Default)]
pub struct Clients(Mutex<State>);

pub fn agent_id(id: Uuid) -> String {
    format!("local:{id}")
}

impl State {
    fn prune(&mut self) {
        self.participants
            .retain(|_, p| !p.peer.is_transport_closed());
        let ids: Vec<_> = self
            .tasks
            .iter()
            .filter(|(_, t)| {
                Instant::now() >= t.deadline
                    || !self.member(t.requester, &t.request.room)
                    || !self.member(t.target, &t.request.room)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(task) = self.tasks.remove(&id) {
                let _ = task.result.send(TaskResult::Error {
                    message: "local participant left, disconnected, or task expired".into(),
                });
            }
        }
    }
    fn member(&self, id: Uuid, room: &str) -> bool {
        self.participants
            .get(&id)
            .is_some_and(|p| !p.peer.is_transport_closed() && p.rooms.contains_key(room))
    }
}

impl Clients {
    pub fn register(&self, id: Uuid, peer: Peer<RoleServer>) {
        let mut state = self.0.lock().unwrap();
        state.prune();
        state.participants.entry(id).or_insert(Participant {
            peer,
            rooms: HashMap::new(),
        });
    }
    pub fn prune(&self) {
        self.0.lock().unwrap().prune();
    }
    pub fn join(&self, id: Uuid, room: &str) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        state.prune();
        let p = state
            .participants
            .get_mut(&id)
            .ok_or_else(|| anyhow!("MCP session is not initialized"))?;
        p.rooms.entry(room.to_owned()).or_default();
        Ok(())
    }
    pub fn require_member(&self, id: Uuid, room: &str) -> Result<()> {
        ensure!(
            self.0.lock().unwrap().member(id, room),
            "join room '{room}' with this session first"
        );
        Ok(())
    }
    pub fn rooms(&self, id: Uuid) -> Vec<String> {
        let mut state = self.0.lock().unwrap();
        state.prune();
        let mut rooms: Vec<_> = state
            .participants
            .get(&id)
            .map(|p| p.rooms.keys().cloned().collect())
            .unwrap_or_default();
        rooms.sort();
        rooms
    }
    pub fn leave(&self, id: Uuid, room: &str) -> usize {
        let mut state = self.0.lock().unwrap();
        if let Some(p) = state.participants.get_mut(&id) {
            p.rooms.remove(room);
        }
        state.prune();
        state
            .participants
            .values()
            .filter(|p| p.rooms.contains_key(room))
            .count()
    }
    pub fn snapshot(&self) -> Vec<Client> {
        let mut state = self.0.lock().unwrap();
        state.prune();
        let mut result: Vec<_> = state
            .participants
            .iter()
            .filter_map(|(id, p)| {
                p.peer.peer_info().map(|info| {
                    let mut rooms: Vec<_> = p.rooms.keys().cloned().collect();
                    rooms.sort();
                    Client {
                        id: agent_id(*id),
                        name: info.client_info.name.clone(),
                        version: info.client_info.version.clone(),
                        rooms,
                    }
                })
            })
            .collect();
        result.sort_by(|a, b| a.id.cmp(&b.id));
        result
    }
    pub fn agents(&self, room: &str, exclude: Option<Uuid>) -> Vec<Agent> {
        let mut state = self.0.lock().unwrap();
        state.prune();
        let mut result: Vec<_> = state
            .participants
            .iter()
            .filter(|(id, p)| Some(**id) != exclude && p.rooms.contains_key(room))
            .map(|(id, p)| Agent {
                name: agent_id(*id),
                agent: p
                    .peer
                    .peer_info()
                    .map(|i| i.client_info.name.clone())
                    .unwrap_or_default(),
                status: p.rooms[room].clone(),
                scope: "local".into(),
                presence: Presence::Online,
                last_seen_secs: 0,
            })
            .collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        result
    }
    pub async fn notify(&self, id: Uuid, room: &str, text: &str) -> Result<()> {
        self.require_member(id, room)?;
        let recipients: Vec<_> = {
            let mut state = self.0.lock().unwrap();
            state.prune();
            let p = state
                .participants
                .get_mut(&id)
                .ok_or_else(|| anyhow!("session disconnected"))?;
            p.rooms.insert(room.into(), Some(text.into()));
            state
                .participants
                .iter()
                .filter(|(other, p)| **other != id && p.rooms.contains_key(room))
                .map(|(_, p)| p.peer.clone())
                .collect()
        };
        for peer in recipients {
            let notification = ServerNotification::CustomNotification(CustomNotification::new(
                "notifications/buddies/status",
                Some(serde_json::json!({"room":room,"author":agent_id(id),"text":text})),
            ));
            let _ =
                tokio::time::timeout(Duration::from_secs(2), peer.send_notification(notification))
                    .await;
        }
        Ok(())
    }
    /// Choose exactly one local executor; None lets the caller use remote P2P.
    pub async fn delegate(
        &self,
        id: Uuid,
        room: &str,
        description: &str,
        seconds: u32,
    ) -> Result<Option<TaskResult>> {
        self.require_member(id, room)?;
        ensure!(
            (1..=300).contains(&seconds),
            "task timeout must be 1..=300 seconds"
        );
        let (request, peer, receiver, deadline) = {
            let mut state = self.0.lock().unwrap();
            state.prune();
            let Some(target) = state
                .participants
                .keys()
                .filter(|target| **target != id && state.member(**target, room))
                .min()
                .copied()
            else {
                return Ok(None);
            };
            ensure!(state.tasks.len() < 128, "too many pending local tasks");
            let peer = state.participants[&target].peer.clone();
            let request = PendingTask {
                task_id: Uuid::new_v4(),
                source_peer: agent_id(id),
                room: room.into(),
                description: description.into(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs(),
                timeout_secs: seconds,
            };
            let deadline = Instant::now() + Duration::from_secs(seconds.into());
            let (result, receiver) = oneshot::channel();
            state.tasks.insert(
                request.task_id,
                Task {
                    request: request.clone(),
                    requester: id,
                    target,
                    deadline,
                    result,
                    polled: false,
                },
            );
            (request, peer, receiver, deadline)
        };
        let _registration = Registration {
            clients: self,
            task: request.task_id,
        };
        let notification = ServerNotification::CustomNotification(CustomNotification::new(
            "notifications/buddies/taskArrived",
            Some(crate::server::task_notification_payload(&request)),
        ));
        // A missing notification stream must not prevent polling the assigned task.
        let _ = tokio::time::timeout_at(
            deadline.min(Instant::now() + Duration::from_secs(2)),
            peer.send_notification(notification),
        )
        .await;
        let result = match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(result)) => result,
            _ => TaskResult::Error {
                message: "local task cancelled or timed out".into(),
            },
        };
        Ok(Some(result))
    }
    pub fn poll(&self, id: Uuid, room: Option<&str>) -> Vec<PendingTask> {
        let mut state = self.0.lock().unwrap();
        state.prune();
        state
            .tasks
            .values_mut()
            .filter(|t| t.target == id && !t.polled && room.is_none_or(|r| r == t.request.room))
            .map(|t| {
                t.polled = true;
                t.request.clone()
            })
            .collect()
    }
    pub fn submit(&self, id: Uuid, task: &PendingTask, result: TaskResult) -> Result<bool> {
        let mut state = self.0.lock().unwrap();
        state.prune();
        let Some(pending) = state.tasks.get(&task.task_id) else {
            ensure!(
                !task.source_peer.starts_with("local:"),
                "local task expired, completed or unknown"
            );
            return Ok(false);
        };
        ensure!(
            pending.target == id
                && pending.request.room == task.room
                && pending.request.source_peer == task.source_peer,
            "task result does not belong to this session and room"
        );
        let pending = state.tasks.remove(&task.task_id).unwrap();
        let _ = pending.result.send(result);
        Ok(true)
    }
}
struct Registration<'a> {
    clients: &'a Clients,
    task: Uuid,
}
impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.clients.0.lock().unwrap().tasks.remove(&self.task);
    }
}
