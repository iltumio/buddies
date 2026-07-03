use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use bytes::Bytes;
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use iroh_gossip::net::Gossip;
use tokio::sync::{Mutex, RwLock, oneshot};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::activity::{DirtySet, FileActivityEntry};
use crate::identity::{LocalSigner, verify_signature};
use crate::memory::{MemoryEntry, SearchFilters};
use crate::protocol::{
    P2PMessage, P2PMessageBody, SignerIdentity, TaskResult, TopicId, room_to_topic,
};
use crate::skill::{SkillEntry, SkillSearchFilters, SkillSearchResult, SkillVote};
use crate::storage::Storage;

const MAX_PENDING_TASKS: usize = 100;

/// How far a signed message's `sent_at` may deviate from local time (in
/// either direction, to tolerate clock skew) before it is dropped as stale.
const MAX_MESSAGE_AGE_SECS: u64 = 600;

/// How many recently seen nonces to remember for replay detection. Only
/// nonces of successfully verified signed messages are tracked, so peers
/// without an accepted signing key cannot evict entries by flooding.
const REPLAY_CACHE_SIZE: usize = 4096;

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn is_message_fresh(sent_at: u64, now: u64) -> bool {
    sent_at.abs_diff(now) <= MAX_MESSAGE_AGE_SECS
}

/// Bounded FIFO set of recently seen message nonces.
struct ReplayGuard {
    seen: HashSet<[u8; 16]>,
    order: VecDeque<[u8; 16]>,
    capacity: usize,
}

impl ReplayGuard {
    fn new(capacity: usize) -> Self {
        Self {
            seen: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns `true` if the nonce was not seen before (and records it),
    /// `false` if it is a replay.
    fn check_and_insert(&mut self, nonce: [u8; 16]) -> bool {
        if !self.seen.insert(nonce) {
            return false;
        }
        self.order.push_back(nonce);
        if self.order.len() > self.capacity
            && let Some(evicted) = self.order.pop_front()
        {
            self.seen.remove(&evicted);
        }
        true
    }
}

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
    pending_skill_searches:
        Arc<Mutex<HashMap<Uuid, tokio::sync::mpsc::Sender<Vec<SkillSearchResult>>>>>,
    incoming_tasks: Arc<Mutex<Vec<PendingTask>>>,
    task_waiters: Arc<Mutex<HashMap<Uuid, oneshot::Sender<TaskResult>>>>,
    task_notify: Arc<tokio::sync::Notify>,
    task_broadcast: tokio::sync::broadcast::Sender<PendingTask>,
    signer: Option<LocalSigner>,
    room_whitelists: Arc<RwLock<HashMap<String, HashSet<SignerIdentity>>>>,
    require_signed: Arc<RwLock<HashMap<String, bool>>>,
    // std Mutex: critical sections are short and never hold across .await
    replay_guard: std::sync::Mutex<ReplayGuard>,
    dirty: Arc<DirtySet>,
    conflict_broadcast: tokio::sync::broadcast::Sender<FileActivityEntry>,
}

impl RoomManager {
    pub fn new(
        gossip: Gossip,
        user_name: String,
        agent_name: String,
        storage: Arc<Storage>,
        signer: Option<LocalSigner>,
        dirty: Arc<DirtySet>,
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
            replay_guard: std::sync::Mutex::new(ReplayGuard::new(REPLAY_CACHE_SIZE)),
            dirty,
            conflict_broadcast: tokio::sync::broadcast::channel(64).0,
        })
    }

    /// Subscribe to task arrival events. Each new `PendingTask` received via
    /// gossip will be sent on the returned channel.
    pub fn subscribe_task_events(&self) -> tokio::sync::broadcast::Receiver<PendingTask> {
        self.task_broadcast.subscribe()
    }

    /// Subscribe to conflict events: fired when a peer's file activity
    /// arrives for a path that is also locally modified in a watched repo.
    #[allow(dead_code)] // consumed by MCP tools in Task 7
    pub fn subscribe_conflict_events(&self) -> tokio::sync::broadcast::Receiver<FileActivityEntry> {
        self.conflict_broadcast.subscribe()
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

        Ok(Self::finalize_memory_results(local_results, 50))
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
                    Self::merge_skill_results(&mut local_results, results);
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

        local_results.sort_by(|a, b| {
            b.rank
                .cmp(&a.rank)
                .then(b.entry.timestamp.cmp(&a.entry.timestamp))
        });
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

        let now = now_unix();

        let msg = P2PMessage::new(P2PMessageBody::TaskRequest {
            task_id,
            source_peer: self.user_name.clone(),
            room: room_name.to_string(),
            description: description.to_string(),
            timeout_secs,
            timestamp: now,
        });

        self.broadcast_to_room(room_name, msg).await?;

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(timeout_secs as u64), rx).await;

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
        let now = now_unix();

        tasks.retain(|t| now < t.timestamp + t.timeout_secs as u64);

        let (matching, remaining): (Vec<_>, Vec<_>) = tasks
            .drain(..)
            .partition(|t| room_filter.is_none() || room_filter == Some(t.room.as_str()));

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

    pub async fn submit_task_result(&self, task: &PendingTask, result: TaskResult) -> Result<()> {
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
                let results = self
                    .storage
                    .search(&query, &filters, 20)
                    .unwrap_or_default();
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
                if !entry.verify_content_hash() {
                    warn!(room = %room_name, skill = %entry.hash, "dropped skill whose hash does not match its content");
                    return;
                }
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
                let now = now_unix();
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
            P2PMessageBody::FileActivity { entry } => {
                if let Err(e) = self.storage.store_file_activity(&entry, now_unix()) {
                    warn!(error = %e, "failed to store received file activity");
                }
                if self.dirty.is_dirty(&entry.repo, &entry.path) {
                    info!(repo = %entry.repo, path = %entry.path, peer = %entry.author, "conflicting file activity from peer");
                    let _ = self.conflict_broadcast.send(entry);
                }
            }
        }
    }

    /// Dedupe merged local + peer memory results by id (memories replicate to
    /// every peer, so the same entry comes back from multiple sources), then
    /// sort newest-first and truncate.
    fn finalize_memory_results(mut results: Vec<MemoryEntry>, limit: usize) -> Vec<MemoryEntry> {
        let mut seen = HashSet::new();
        results.retain(|e| seen.insert(e.id));
        results.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
        results.truncate(limit);
        results
    }

    /// Merge peer skill results into the accumulated list. Votes replicate to
    /// every peer via gossip, so each peer's rank already reflects the full
    /// vote set — take the max instead of summing to avoid double-counting.
    fn merge_skill_results(local: &mut Vec<SkillSearchResult>, incoming: Vec<SkillSearchResult>) {
        for result in incoming {
            if let Some(existing) = local.iter_mut().find(|r| r.entry.hash == result.entry.hash) {
                existing.rank = existing.rank.max(result.rank);
            } else {
                local.push(result);
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
            Ok(true) => {}
            Ok(false) => {
                warn!(room = %room_name, identity = %identity.to_label(), "signature verification failed");
                return false;
            }
            Err(error) => {
                warn!(room = %room_name, identity = %identity.to_label(), %error, "signature verification errored");
                return false;
            }
        }

        // Replay protection, only after the signature checks out so that
        // unverified traffic cannot poison the nonce cache.
        if !is_message_fresh(msg.sent_at, now_unix()) {
            warn!(room = %room_name, identity = %identity.to_label(), sent_at = msg.sent_at, "dropped signed message outside freshness window");
            return false;
        }
        if !self
            .replay_guard
            .lock()
            .expect("replay guard lock poisoned")
            .check_and_insert(msg.nonce)
        {
            warn!(room = %room_name, identity = %identity.to_label(), "dropped replayed signed message");
            return false;
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryKind;

    fn nonce(n: u8) -> [u8; 16] {
        [n; 16]
    }

    #[test]
    fn replay_guard_rejects_previously_seen_nonces() {
        let mut guard = ReplayGuard::new(8);
        assert!(guard.check_and_insert(nonce(1)));
        assert!(guard.check_and_insert(nonce(2)));
        assert!(!guard.check_and_insert(nonce(1)));
        assert!(!guard.check_and_insert(nonce(2)));
    }

    #[test]
    fn replay_guard_evicts_oldest_nonce_beyond_capacity() {
        let mut guard = ReplayGuard::new(2);
        assert!(guard.check_and_insert(nonce(1)));
        assert!(guard.check_and_insert(nonce(2)));
        assert!(guard.check_and_insert(nonce(3))); // evicts nonce 1
        assert!(guard.check_and_insert(nonce(1))); // forgotten, accepted again
        assert!(!guard.check_and_insert(nonce(3))); // still tracked
    }

    #[test]
    fn message_freshness_window_covers_skew_in_both_directions() {
        let now = 1_000_000;
        assert!(is_message_fresh(now, now));
        assert!(is_message_fresh(now - MAX_MESSAGE_AGE_SECS, now));
        assert!(is_message_fresh(now + MAX_MESSAGE_AGE_SECS, now));
        assert!(!is_message_fresh(now - MAX_MESSAGE_AGE_SECS - 1, now));
        assert!(!is_message_fresh(now + MAX_MESSAGE_AGE_SECS + 1, now));
        // near-epoch sent_at must not underflow
        assert!(!is_message_fresh(0, now));
    }

    fn memory(id: &str, timestamp: u64) -> MemoryEntry {
        MemoryEntry {
            id: Uuid::parse_str(id).expect("valid uuid"),
            author: "tester".into(),
            timestamp,
            room: "room-a".into(),
            kind: MemoryKind::Context,
            title: format!("entry-{timestamp}"),
            content: "content".into(),
            tags: vec![],
            references: vec![],
        }
    }

    fn skill_result(hash: &str, rank: i64) -> SkillSearchResult {
        SkillSearchResult {
            entry: SkillEntry {
                hash: hash.into(),
                author: "tester".into(),
                timestamp: 0,
                room: "room-a".into(),
                title: hash.into(),
                content: "content".into(),
                tags: vec![],
                version: 1,
                parent_hash: None,
                signed_by: None,
                signature: None,
            },
            rank,
        }
    }

    #[test]
    fn finalize_memory_results_dedupes_by_id_and_sorts_newest_first() {
        let a = memory("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", 1);
        let b = memory("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", 2);

        // Memories replicate to every peer, so a room search typically gets
        // the same entry back from the local store and from each peer.
        let merged = vec![a.clone(), b.clone(), a.clone(), b.clone()];
        let results = RoomManager::finalize_memory_results(merged, 50);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, b.id);
        assert_eq!(results[1].id, a.id);
    }

    #[test]
    fn merge_skill_results_takes_max_rank_instead_of_summing() {
        let mut local = vec![skill_result("hash-a", 3)];

        // Peer's rank reflects the same replicated votes, plus one vote we
        // have not seen yet.
        RoomManager::merge_skill_results(
            &mut local,
            vec![skill_result("hash-a", 4), skill_result("hash-b", 1)],
        );

        assert_eq!(local.len(), 2);
        assert_eq!(local[0].entry.hash, "hash-a");
        assert_eq!(local[0].rank, 4);
        assert_eq!(local[1].entry.hash, "hash-b");
        assert_eq!(local[1].rank, 1);
    }
}
