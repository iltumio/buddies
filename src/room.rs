use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use bytes::Bytes;
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use iroh_gossip::net::Gossip;
use tokio::sync::{Mutex, RwLock, oneshot};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::identity::{LocalSigner, verify_signature};
use crate::memory::{MemoryEntry, SearchFilters};
use crate::protocol::{
    P2PMessage, P2PMessageBody, SignerIdentity, TaskResult, TopicId, room_to_topic,
};
use crate::skill::{SkillEntry, SkillSearchFilters, SkillSearchResult, SkillVote};
use crate::storage::Storage;

const MAX_PENDING_TASKS: usize = 100;

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub name: String,
    pub agent: String,
    pub last_status: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingTask {
    pub task_id: Uuid,
    pub source_peer: String,
    pub room: String,
    pub description: String,
    pub timestamp: u64,
    pub timeout_secs: u32,
}

struct RoomInner {
    sender: GossipSender,
    _receiver_handle: tokio::task::JoinHandle<()>,
}

pub struct RoomManager {
    gossip: Gossip,
    user_name: String,
    agent_name: String,
    rooms: RwLock<HashMap<String, RoomInner>>,
    peers: Arc<RwLock<HashMap<String, HashMap<String, PeerInfo>>>>,
    storage: Arc<Storage>,
    pending_searches: Arc<Mutex<HashMap<Uuid, tokio::sync::mpsc::Sender<Vec<MemoryEntry>>>>>,
    pending_skill_searches: Arc<Mutex<HashMap<Uuid, tokio::sync::mpsc::Sender<Vec<SkillSearchResult>>>>>,
    incoming_tasks: Arc<Mutex<Vec<PendingTask>>>,
    task_waiters: Arc<Mutex<HashMap<Uuid, oneshot::Sender<TaskResult>>>>,
    task_notify: Arc<tokio::sync::Notify>,
    task_broadcast: tokio::sync::broadcast::Sender<PendingTask>,
    signer: Option<LocalSigner>,
    room_whitelists: Arc<RwLock<HashMap<String, HashSet<SignerIdentity>>>>,
    require_signed: Arc<RwLock<HashMap<String, bool>>>,
}

impl RoomManager {
    pub fn new(
        gossip: Gossip,
        user_name: String,
        agent_name: String,
        storage: Arc<Storage>,
        signer: Option<LocalSigner>,
    ) -> Arc<Self> {
        Arc::new(Self {
            gossip,
            user_name,
            agent_name,
            rooms: RwLock::new(HashMap::new()),
            peers: Arc::new(RwLock::new(HashMap::new())),
            storage,
            pending_searches: Arc::new(Mutex::new(HashMap::new())),
            pending_skill_searches: Arc::new(Mutex::new(HashMap::new())),
            incoming_tasks: Arc::new(Mutex::new(Vec::new())),
            task_waiters: Arc::new(Mutex::new(HashMap::new())),
            task_notify: Arc::new(tokio::sync::Notify::new()),
            task_broadcast: tokio::sync::broadcast::channel(64).0,
            signer,
            room_whitelists: Arc::new(RwLock::new(HashMap::new())),
            require_signed: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Subscribe to task arrival events. Each new `PendingTask` received via
    /// gossip will be sent on the returned channel.
    pub fn subscribe_task_events(&self) -> tokio::sync::broadcast::Receiver<PendingTask> {
        self.task_broadcast.subscribe()
    }

    pub fn signer_identity_label(&self) -> Option<String> {
        self.signer.as_ref().map(|s| s.identity().to_label())
    }

    /// Sign a skill entry in place using the local signer (if configured).
    pub fn try_sign_skill(&self, entry: &mut SkillEntry) {
        let Some(signer) = self.signer.as_ref() else {
            return;
        };
        let payload = entry.signing_payload();
        match signer.sign(&payload) {
            Ok(signature) => {
                entry.signed_by = Some(signer.identity());
                entry.signature = Some(signature);
            }
            Err(error) => {
                warn!(%error, "failed to sign skill; publishing unsigned");
            }
        }
    }

    /// Verify the embedded signature on a skill entry.
    /// Returns `true` if the signature is valid or absent (unsigned skills are
    /// accepted unless room policy rejects them).
    pub fn verify_skill_signature(&self, room_name: &str, entry: &SkillEntry) -> bool {
        let Some(identity) = entry.signed_by.as_ref() else {
            return true; // unsigned — room policy decides acceptance
        };
        let Some(signature) = entry.signature.as_ref() else {
            warn!(room = %room_name, skill = %entry.hash, "skill has signer but no signature");
            return false;
        };
        let payload = entry.signing_payload();
        match verify_signature(identity, &payload, signature) {
            Ok(true) => true,
            Ok(false) => {
                warn!(room = %room_name, skill = %entry.hash, identity = %identity.to_label(), "skill signature verification failed");
                false
            }
            Err(error) => {
                warn!(room = %room_name, skill = %entry.hash, %error, "skill signature verification errored");
                false
            }
        }
    }

    pub async fn set_identity_policy(
        &self,
        room_name: &str,
        identities: Vec<SignerIdentity>,
        require_signed: bool,
    ) {
        {
            let mut whitelists = self.room_whitelists.write().await;
            whitelists.insert(room_name.to_string(), identities.into_iter().collect());
        }
        {
            let mut modes = self.require_signed.write().await;
            modes.insert(room_name.to_string(), require_signed);
        }
    }

    pub async fn add_whitelisted_identity(&self, room_name: &str, identity: SignerIdentity) {
        let mut whitelists = self.room_whitelists.write().await;
        let whitelist = whitelists.entry(room_name.to_string()).or_default();
        whitelist.insert(identity);
    }

    pub async fn get_identity_policy(&self, room_name: &str) -> (Vec<String>, bool) {
        let whitelist = {
            let whitelists = self.room_whitelists.read().await;
            whitelists
                .get(room_name)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|id| id.to_label())
                .collect::<Vec<_>>()
        };
        let require_signed = {
            let modes = self.require_signed.read().await;
            *modes.get(room_name).unwrap_or(&false)
        };
        (whitelist, require_signed)
    }

    #[allow(dead_code)]
    pub fn peer_id(&self) -> &str {
        &self.user_name
    }

    pub async fn join_room(
        self: &Arc<Self>,
        room_name: &str,
        bootstrap_peers: Vec<iroh::EndpointId>,
    ) -> Result<TopicId> {
        let topic_id = room_to_topic(room_name);

        {
            let rooms = self.rooms.read().await;
            if rooms.contains_key(room_name) {
                return Ok(topic_id);
            }
        }

        let topic = if bootstrap_peers.is_empty() {
            self.gossip.subscribe(topic_id, bootstrap_peers).await?
        } else {
            self.gossip
                .subscribe_and_join(topic_id, bootstrap_peers)
                .await?
        };

        let (sender, receiver) = topic.split();

        let join_msg = P2PMessage::new(P2PMessageBody::Join {
            name: self.user_name.clone(),
            agent: self.agent_name.clone(),
        });
        sender.broadcast(join_msg.to_bytes()).await?;

        let room_name_owned = room_name.to_string();
        let manager = Arc::clone(self);
        let receiver_handle = tokio::spawn(async move {
            if let Err(e) = manager.receive_loop(&room_name_owned, receiver).await {
                warn!(room = %room_name_owned, error = %e, "room receive loop ended");
            }
        });

        {
            let mut peers = self.peers.write().await;
            peers.entry(room_name.to_string()).or_default();
        }

        {
            let mut rooms = self.rooms.write().await;
            rooms.insert(
                room_name.to_string(),
                RoomInner {
                    sender,
                    _receiver_handle: receiver_handle,
                },
            );
        }

        Ok(topic_id)
    }

    pub async fn leave_room(&self, room_name: &str) -> Result<()> {
        let room = {
            let mut rooms = self.rooms.write().await;
            rooms.remove(room_name)
        };

        if let Some(room) = room {
            let leave_msg = P2PMessage::new(P2PMessageBody::Leave {
                name: self.user_name.clone(),
            });
            let _ = room.sender.broadcast(leave_msg.to_bytes()).await;
            room._receiver_handle.abort();
        }

        {
            let mut peers = self.peers.write().await;
            peers.remove(room_name);
        }

        Ok(())
    }

    pub async fn list_rooms(&self) -> Vec<String> {
        let rooms = self.rooms.read().await;
        rooms.keys().cloned().collect()
    }

    pub async fn get_room_peers(&self, room_name: &str) -> HashMap<String, PeerInfo> {
        let peers = self.peers.read().await;
        peers.get(room_name).cloned().unwrap_or_default()
    }

    pub async fn broadcast_to_room(&self, room_name: &str, msg: P2PMessage) -> Result<()> {
        let msg = self.try_sign_message(msg);
        let rooms = self.rooms.read().await;
        let room = rooms
            .get(room_name)
            .ok_or_else(|| anyhow::anyhow!("not in room: {room_name}"))?;
        room.sender.broadcast(msg.to_bytes()).await?;
        Ok(())
    }

    fn try_sign_message(&self, mut msg: P2PMessage) -> P2PMessage {
        let Some(signer) = self.signer.as_ref() else {
            return msg;
        };
        let payload = msg.signing_payload();
        match signer.sign(&payload) {
            Ok(signature) => {
                msg.signed_by = Some(signer.identity());
                msg.signature = Some(signature);
                msg
            }
            Err(error) => {
                warn!(%error, "failed to sign outgoing message; sending unsigned");
                msg
            }
        }
    }

    pub async fn search_distributed(
        &self,
        room_name: &str,
        query: &str,
        filters: &SearchFilters,
        timeout_secs: u64,
    ) -> Result<Vec<MemoryEntry>> {
        let mut local_results = self.storage.search(query, filters, 50)?;

        let request_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<MemoryEntry>>(32);

        {
            let mut pending = self.pending_searches.lock().await;
            pending.insert(request_id, tx);
        }

        let search_msg = P2PMessage::new(P2PMessageBody::SearchRequest {
            request_id,
            query: query.to_string(),
            filters: filters.clone(),
        });

        if let Err(e) = self.broadcast_to_room(room_name, search_msg).await {
            debug!(error = %e, "no peers to search (broadcasting failed)");
        }

        let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                Some(results) = rx.recv() => {
                    local_results.extend(results);
                }
                () = &mut deadline => {
                    break;
                }
            }
        }

        {
            let mut pending = self.pending_searches.lock().await;
            pending.remove(&request_id);
        }

        local_results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        local_results.truncate(50);

        Ok(local_results)
    }

    pub async fn search_skills_distributed(
        &self,
        room_name: &str,
        query: &str,
        filters: &SkillSearchFilters,
        timeout_secs: u64,
    ) -> Result<Vec<SkillSearchResult>> {
        let mut local_results = self.storage.search_skills(query, filters, 50)?;

        let request_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<SkillSearchResult>>(32);

        {
            let mut pending = self.pending_skill_searches.lock().await;
            pending.insert(request_id, tx);
        }

        let search_msg = P2PMessage::new(P2PMessageBody::SkillSearchRequest {
            request_id,
            query: query.to_string(),
            filters: filters.clone(),
        });

        if let Err(e) = self.broadcast_to_room(room_name, search_msg).await {
            debug!(error = %e, "no peers to search skills (broadcasting failed)");
        }

        let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                Some(results) = rx.recv() => {
                    for result in results {
                        if let Some(existing) = local_results.iter_mut().find(|r| r.entry.hash == result.entry.hash) {
                            existing.rank += result.rank;
                        } else {
                            local_results.push(result);
                        }
                    }
                }
                () = &mut deadline => {
                    break;
                }
            }
        }

        {
            let mut pending = self.pending_skill_searches.lock().await;
            pending.remove(&request_id);
        }

        local_results.sort_by(|a, b| b.rank.cmp(&a.rank).then(b.entry.timestamp.cmp(&a.entry.timestamp)));
        local_results.truncate(50);

        Ok(local_results)
    }

    pub async fn delegate_task(
        &self,
        room_name: &str,
        description: &str,
        timeout_secs: u32,
    ) -> Result<TaskResult> {
        let task_id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel::<TaskResult>();

        {
            let mut waiters = self.task_waiters.lock().await;
            waiters.insert(task_id, tx);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let msg = P2PMessage::new(P2PMessageBody::TaskRequest {
            task_id,
            source_peer: self.user_name.clone(),
            room: room_name.to_string(),
            description: description.to_string(),
            timeout_secs,
            timestamp: now,
        });

        self.broadcast_to_room(room_name, msg).await?;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs as u64),
            rx,
        )
        .await;

        {
            let mut waiters = self.task_waiters.lock().await;
            waiters.remove(&task_id);
        }

        match result {
            Ok(Ok(task_result)) => Ok(task_result),
            Ok(Err(_)) => Ok(TaskResult::Error {
                message: "task response channel closed unexpectedly".into(),
            }),
            Err(_) => Ok(TaskResult::Error {
                message: format!("no peer completed the task within {timeout_secs}s"),
            }),
        }
    }

    pub async fn poll_tasks(&self, room_filter: Option<&str>) -> Vec<PendingTask> {
        let mut tasks = self.incoming_tasks.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        tasks.retain(|t| now < t.timestamp + t.timeout_secs as u64);

        let (matching, remaining): (Vec<_>, Vec<_>) = tasks.drain(..).partition(|t| {
            room_filter.is_none() || room_filter == Some(t.room.as_str())
        });

        *tasks = remaining;
        matching
    }

    pub async fn wait_for_tasks(
        &self,
        room_filter: Option<&str>,
        timeout_secs: u64,
    ) -> Vec<PendingTask> {
        let immediate = self.poll_tasks(room_filter).await;
        if !immediate.is_empty() {
            return immediate;
        }

        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            self.task_notify.notified(),
        )
        .await;

        self.poll_tasks(room_filter).await
    }

    pub async fn submit_task_result(
        &self,
        task: &PendingTask,
        result: TaskResult,
    ) -> Result<()> {
        let msg = P2PMessage::new(P2PMessageBody::TaskResponse {
            task_id: task.task_id,
            result,
            completed_by: self.user_name.clone(),
        });
        self.broadcast_to_room(&task.room, msg).await
    }

    async fn receive_loop(&self, room_name: &str, mut receiver: GossipReceiver) -> Result<()> {
        use n0_future::TryStreamExt;

        while let Some(event) = receiver.try_next().await? {
            if let Event::Received(msg) = event {
                self.handle_message(room_name, &msg.content).await;
            }
        }
        Ok(())
    }

    async fn handle_message(&self, room_name: &str, content: &Bytes) {
        let msg = match P2PMessage::from_bytes(content) {
            Ok(m) => m,
            Err(e) => {
                debug!(error = %e, "failed to decode P2P message");
                return;
            }
        };

        if !self.verify_incoming_message(room_name, &msg).await {
            return;
        }

        match msg.body {
            P2PMessageBody::Join { name, agent } => {
                let is_new = {
                    let mut peers = self.peers.write().await;
                    let room_peers = peers.entry(room_name.to_string()).or_default();
                    let is_new = !room_peers.contains_key(&name);
                    room_peers.insert(
                        name.clone(),
                        PeerInfo {
                            name,
                            agent,
                            last_status: None,
                        },
                    );
                    is_new
                };

                // Re-broadcast our own Join so the new peer discovers us
                if is_new {
                    let join_msg = P2PMessage::new(P2PMessageBody::Join {
                        name: self.user_name.clone(),
                        agent: self.agent_name.clone(),
                    });
                    if let Err(e) = self.broadcast_to_room(room_name, join_msg).await {
                        debug!(room = %room_name, error = %e, "failed to re-broadcast join");
                    }
                }
            }
            P2PMessageBody::Leave { name } => {
                let mut peers = self.peers.write().await;
                if let Some(room_peers) = peers.get_mut(room_name) {
                    room_peers.remove(&name);
                }
            }
            P2PMessageBody::MemoryCreated { entry } => {
                if let Err(e) = self.storage.store(&entry) {
                    warn!(error = %e, "failed to store received memory");
                }
            }
            P2PMessageBody::StatusUpdate { author, text } => {
                let mut peers = self.peers.write().await;
                if let Some(room_peers) = peers.get_mut(room_name)
                    && let Some(peer) = room_peers.get_mut(&author)
                {
                    peer.last_status = Some(text);
                }
            }
            P2PMessageBody::SearchRequest {
                request_id,
                query,
                filters,
            } => {
                let results = self.storage.search(&query, &filters, 20).unwrap_or_default();
                if !results.is_empty() {
                    let response = P2PMessage::new(P2PMessageBody::SearchResponse {
                        request_id,
                        results,
                        peer_name: self.user_name.clone(),
                    });
                    if let Err(e) = self.broadcast_to_room(room_name, response).await {
                        debug!(error = %e, "failed to send search response");
                    }
                }
            }
            P2PMessageBody::SearchResponse {
                request_id,
                results,
                ..
            } => {
                let pending = self.pending_searches.lock().await;
                if let Some(tx) = pending.get(&request_id) {
                    let _ = tx.send(results).await;
                }
            }
            P2PMessageBody::TaskRequest {
                task_id,
                source_peer,
                room,
                description,
                timeout_secs,
                timestamp,
            } => {
                if source_peer == self.user_name {
                    return;
                }
                info!(task_id = %task_id, from = %source_peer, "received delegated task");
                let mut tasks = self.incoming_tasks.lock().await;
                if tasks.len() >= MAX_PENDING_TASKS {
                    warn!("incoming task queue full, dropping task {task_id}");
                    return;
                }
                let task = PendingTask {
                    task_id,
                    source_peer,
                    room,
                    description,
                    timestamp,
                    timeout_secs,
                };
                let task_clone = task.clone();
                tasks.push(task);
                drop(tasks);
                self.task_notify.notify_waiters();
                let _ = self.task_broadcast.send(task_clone);
            }
            P2PMessageBody::TaskClaimed {
                task_id,
                claimed_by,
            } => {
                debug!(task_id = %task_id, claimed_by = %claimed_by, "task claimed");
            }
            P2PMessageBody::TaskResponse {
                task_id,
                result,
                completed_by,
            } => {
                info!(task_id = %task_id, by = %completed_by, "received task result");
                let mut waiters = self.task_waiters.lock().await;
                if let Some(tx) = waiters.remove(&task_id) {
                    let _ = tx.send(result);
                }
            }
            P2PMessageBody::SkillPublished { entry } => {
                if !self.verify_skill_signature(room_name, &entry) {
                    warn!(room = %room_name, skill = %entry.hash, "dropped skill with invalid signature");
                    return;
                }
                if let Err(e) = self.storage.store_skill(&entry) {
                    warn!(error = %e, "failed to store received skill");
                }
            }
            P2PMessageBody::SkillSearchRequest {
                request_id,
                query,
                filters,
            } => {
                let results = self
                    .storage
                    .search_skills(&query, &filters, 20)
                    .unwrap_or_default();
                if !results.is_empty() {
                    let response = P2PMessage::new(P2PMessageBody::SkillSearchResponse {
                        request_id,
                        results,
                        peer_name: self.user_name.clone(),
                    });
                    if let Err(e) = self.broadcast_to_room(room_name, response).await {
                        debug!(error = %e, "failed to send skill search response");
                    }
                }
            }
            P2PMessageBody::SkillSearchResponse {
                request_id,
                results,
                ..
            } => {
                let pending = self.pending_skill_searches.lock().await;
                if let Some(tx) = pending.get(&request_id) {
                    let _ = tx.send(results).await;
                }
            }
            P2PMessageBody::SkillVoteCast {
                skill_hash,
                voter,
                score,
            } => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let vote = SkillVote {
                    skill_hash,
                    voter,
                    score,
                    timestamp: now,
                };
                if let Err(e) = self.storage.vote_skill(&vote) {
                    warn!(error = %e, "failed to store received skill vote");
                }
            }
        }
    }

    async fn verify_incoming_message(&self, room_name: &str, msg: &P2PMessage) -> bool {
        let whitelist = {
            let whitelists = self.room_whitelists.read().await;
            whitelists.get(room_name).cloned().unwrap_or_default()
        };
        let must_be_signed = {
            let modes = self.require_signed.read().await;
            *modes.get(room_name).unwrap_or(&false)
        };

        let Some(identity) = msg.signed_by.as_ref() else {
            if must_be_signed || !whitelist.is_empty() {
                warn!(room = %room_name, "dropped unsigned message due to identity policy");
                return false;
            }
            return true;
        };

        let Some(signature) = msg.signature.as_ref() else {
            warn!(room = %room_name, identity = %identity.to_label(), "dropped unsigned payload");
            return false;
        };

        if !whitelist.is_empty() && !whitelist.contains(identity) {
            warn!(room = %room_name, identity = %identity.to_label(), "identity not in whitelist");
            return false;
        }

        let payload = msg.signing_payload();
        match verify_signature(identity, &payload, signature) {
            Ok(true) => true,
            Ok(false) => {
                warn!(room = %room_name, identity = %identity.to_label(), "signature verification failed");
                false
            }
            Err(error) => {
                warn!(room = %room_name, identity = %identity.to_label(), %error, "signature verification errored");
                false
            }
        }
    }
}
