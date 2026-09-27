use crate::resilience::{Backoff, Presence, PresenceConfig};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

use anyhow::Result;
use bytes::Bytes;
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use iroh_gossip::net::Gossip;
use tokio::sync::{Mutex, RwLock, oneshot};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::activity::{ConflictEvent, DirtySet};
use crate::async_storage::AsyncStorage;
use crate::identity::{LocalSigner, verify_signature};
use crate::memory::{MemoryEntry, SearchFilters};
use crate::pending::PendingRequests;
use crate::protocol::{
    P2PMessage, P2PMessageBody, SignerIdentity, TaskResult, TopicId, room_to_topic,
};
use crate::skill::{SkillEntry, SkillSearchFilters, SkillSearchResult, SkillVote};

const MAX_PENDING_TASKS: usize = 100;
const MAX_SEARCH_SECONDS: u64 = 30;
const MAX_TASK_SECONDS: u32 = 300;
const MAX_SEARCH_RESULTS: usize = 50;

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
    signed_by: Option<SignerIdentity>,
    pub last_seen: Instant,
}

impl PeerInfo {
    fn new(name: String, agent: String, signed_by: Option<SignerIdentity>) -> Self {
        Self {
            name,
            agent,
            last_status: None,
            last_seen: Instant::now(),
            signed_by,
        }
    }

    pub fn presence(&self, config: PresenceConfig) -> Presence {
        config.state(self.last_seen.elapsed())
    }

    fn accepts_identity(&self, identity: Option<&SignerIdentity>) -> bool {
        self.signed_by.as_ref() == identity
    }
}

fn peer_action_is_authenticated(
    room_peers: &HashMap<String, PeerInfo>,
    peer_name: &str,
    signed_by: Option<&SignerIdentity>,
) -> bool {
    room_peers
        .get(peer_name)
        .is_some_and(|peer| peer.accepts_identity(signed_by))
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoomHealth {
    pub state: String,
    pub neighbors: usize,
    pub reconnect_attempts: u64,
    pub last_error: Option<String>,
}

impl Default for RoomHealth {
    fn default() -> Self {
        Self {
            state: "waiting".into(),
            neighbors: 0,
            reconnect_attempts: 0,
            last_error: None,
        }
    }
}

struct RoomInner {
    sender: Option<GossipSender>,
    health: Arc<RwLock<RoomHealth>>,
    _receiver_handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct MemorySearch {
    sender: tokio::sync::mpsc::Sender<Vec<MemoryEntry>>,
    query: String,
    filters: SearchFilters,
}

#[derive(Clone)]
struct SkillSearch {
    sender: tokio::sync::mpsc::Sender<Vec<SkillSearchResult>>,
    query: String,
    filters: SkillSearchFilters,
}

pub struct RoomManager {
    gossip: Gossip,
    lifecycle: Mutex<()>,
    pub presence: PresenceConfig,
    closing: std::sync::atomic::AtomicBool,
    user_name: String,
    agent_name: String,
    rooms: RwLock<HashMap<String, RoomInner>>,
    peers: Arc<RwLock<HashMap<String, HashMap<String, PeerInfo>>>>,
    storage: Arc<AsyncStorage>,
    pending_searches: PendingRequests<MemorySearch>,
    pending_skill_searches: PendingRequests<SkillSearch>,
    incoming_tasks: Arc<Mutex<Vec<PendingTask>>>,
    task_waiters: PendingRequests<oneshot::Sender<TaskResult>>,
    task_notify: Arc<tokio::sync::Notify>,
    task_broadcast: tokio::sync::broadcast::Sender<PendingTask>,
    signer: Option<LocalSigner>,
    room_whitelists: Arc<RwLock<HashMap<String, HashSet<SignerIdentity>>>>,
    require_signed: Arc<RwLock<HashMap<String, bool>>>,
    // std Mutex: critical sections are short and never hold across .await
    replay_guard: std::sync::Mutex<ReplayGuard>,
    dirty: Arc<DirtySet>,
    conflict_broadcast: tokio::sync::broadcast::Sender<ConflictEvent>,
}

impl RoomManager {
    pub fn new(
        gossip: Gossip,
        user_name: String,
        agent_name: String,
        storage: Arc<AsyncStorage>,
        signer: Option<LocalSigner>,
        dirty: Arc<DirtySet>,
        presence: PresenceConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            gossip,
            lifecycle: Mutex::new(()),
            presence,
            closing: std::sync::atomic::AtomicBool::new(false),
            user_name,
            agent_name,
            rooms: RwLock::new(HashMap::new()),
            peers: Arc::new(RwLock::new(HashMap::new())),
            storage,
            pending_searches: PendingRequests::default(),
            pending_skill_searches: PendingRequests::default(),
            incoming_tasks: Arc::new(Mutex::new(Vec::new())),
            task_waiters: PendingRequests::default(),
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
    pub fn subscribe_conflict_events(&self) -> tokio::sync::broadcast::Receiver<ConflictEvent> {
        self.conflict_broadcast.subscribe()
    }

    pub fn signer_identity_label(&self) -> Option<String> {
        self.signer.as_ref().map(|s| s.identity().to_label())
    }

    pub fn voter_identity_label(&self) -> Option<String> {
        self.signer.as_ref()?.identity().voting_label()
    }

    /// An explicitly configured signer must succeed; never silently downgrade.
    pub async fn try_sign_skill(&self, entry: &mut SkillEntry) -> Result<()> {
        if let Some(signer) = &self.signer {
            entry.signature = Some(signer.sign(&entry.signing_payload()).await?);
            entry.signed_by = Some(signer.identity());
        }
        Ok(())
    }

    async fn validate_skill(&self, room_name: &str, entry: &SkillEntry) -> bool {
        let (whitelist, require_signed) = self.get_identity_policy(room_name).await;
        crate::validation::validate_skill(room_name, entry, &whitelist, require_signed).await
    }

    async fn memory_results_for_peer(
        &self,
        room: &str,
        query: &str,
        mut filters: SearchFilters,
    ) -> Result<Vec<MemoryEntry>> {
        filters.room = Some(room.to_owned());
        self.storage.search(query, &filters, 20).await
    }

    async fn skill_results_for_peer(
        &self,
        room: &str,
        query: &str,
        mut filters: SkillSearchFilters,
    ) -> Result<Vec<SkillSearchResult>> {
        filters.room = Some(room.to_owned());
        self.storage.search_skills(query, &filters, 20).await
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
        let _operation = self.lifecycle.lock().await;
        anyhow::ensure!(
            !self.closing.load(std::sync::atomic::Ordering::SeqCst),
            "node is shutting down"
        );
        self.presence.validate()?;
        let topic_id = room_to_topic(room_name);
        if self.rooms.read().await.contains_key(room_name) {
            return Ok(topic_id);
        }
        // Subscription must not wait indefinitely for an unreachable bootstrap peer.
        let topic = tokio::time::timeout(
            Duration::from_secs(5),
            self.gossip.subscribe(topic_id, bootstrap_peers.clone()),
        )
        .await??;
        let (sender, receiver) = topic.split();
        let health = Arc::new(RwLock::new(RoomHealth::default()));
        let name = room_name.to_owned();
        let manager = Arc::clone(self);
        let room_health = health.clone();
        let initial_sender = sender.clone();
        // Publish the room before its receiver can process a Join and reply.
        let (start_tx, start_rx) = oneshot::channel();
        let receiver_handle = tokio::spawn(async move {
            if start_rx.await.is_ok() {
                manager
                    .supervise_room(
                        &name,
                        bootstrap_peers,
                        initial_sender,
                        receiver,
                        room_health,
                    )
                    .await;
            }
        });
        self.peers
            .write()
            .await
            .entry(room_name.to_owned())
            .or_default();
        self.rooms.write().await.insert(
            room_name.to_owned(),
            RoomInner {
                sender: Some(sender),
                health,
                _receiver_handle: receiver_handle,
            },
        );
        let _ = start_tx.send(());
        Ok(topic_id)
    }

    pub async fn leave_room(&self, room_name: &str) -> Result<()> {
        let _operation = self.lifecycle.lock().await;
        let room = {
            let mut rooms = self.rooms.write().await;
            rooms.remove(room_name)
        };

        if let Some(room) = room {
            let leave_msg = P2PMessage::new(P2PMessageBody::Leave {
                name: self.user_name.clone(),
            });
            // Stop supervision before announcing departure, so it cannot rejoin.
            room._receiver_handle.abort();
            if let Some(sender) = room.sender {
                let _ = tokio::time::timeout(Duration::from_secs(2), async {
                    let msg = self.try_sign_message(leave_msg).await?;
                    sender.broadcast(msg.to_bytes()).await?;
                    Ok::<_, anyhow::Error>(())
                })
                .await;
            }
            let _ = room._receiver_handle.await;
        }

        {
            let mut peers = self.peers.write().await;
            peers.remove(room_name);
        }

        Ok(())
    }

    pub async fn shutdown(&self) {
        let _operation = self.lifecycle.lock().await;
        self.closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let rooms = std::mem::take(&mut *self.rooms.write().await);
        for room in rooms.values() {
            room._receiver_handle.abort();
        }
        for (_, room) in rooms {
            let _ = room._receiver_handle.await;
        }
        self.peers.write().await.clear();
        self.pending_searches.clear();
        self.pending_skill_searches.clear();
        self.task_waiters.clear();
        self.incoming_tasks.lock().await.clear();
    }

    pub async fn is_joined(&self, room_name: &str) -> bool {
        let rooms = self.rooms.read().await;
        rooms.contains_key(room_name)
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
        let sender = self
            .rooms
            .read()
            .await
            .get(room_name)
            .ok_or_else(|| anyhow::anyhow!("not in room: {room_name}"))?
            .sender
            .clone()
            .ok_or_else(|| anyhow::anyhow!("room is reconnecting: {room_name}"))?;
        let msg = self.try_sign_message(msg).await?;
        let bytes = msg.to_bytes();
        anyhow::ensure!(
            bytes.len() <= crate::node::GOSSIP_MAX_MESSAGE_SIZE,
            "gossip message exceeds size limit"
        );
        tokio::time::timeout(Duration::from_secs(5), sender.broadcast(bytes)).await??;
        Ok(())
    }

    async fn try_sign_message(&self, mut msg: P2PMessage) -> Result<P2PMessage> {
        if let Some(signer) = &self.signer {
            msg.signature = Some(signer.sign(&msg.signing_payload()).await?);
            msg.signed_by = Some(signer.identity());
        }
        Ok(msg)
    }

    pub async fn search_distributed(
        &self,
        room_name: &str,
        query: &str,
        filters: &SearchFilters,
        timeout_secs: u64,
    ) -> Result<Vec<MemoryEntry>> {
        anyhow::ensure!(
            timeout_secs <= MAX_SEARCH_SECONDS,
            "search timeout exceeds 30 seconds"
        );
        let mut filters = filters.clone();
        filters.room = Some(room_name.to_owned());
        let mut local_results = self.storage.search(query, &filters, 50).await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

        let request_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<MemoryEntry>>(32);

        let _registration = self.pending_searches.register(
            request_id,
            room_name,
            MemorySearch {
                sender: tx,
                query: query.to_owned(),
                filters: filters.clone(),
            },
        )?;

        let search_msg = P2PMessage::new(P2PMessageBody::SearchRequest {
            request_id,
            query: query.to_string(),
            filters: filters.clone(),
        });

        if !matches!(
            tokio::time::timeout_at(deadline, self.broadcast_to_room(room_name, search_msg)).await,
            Ok(Ok(()))
        ) {
            return Ok(local_results);
        }

        let deadline = tokio::time::sleep_until(deadline);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                Some(results) = rx.recv() => {
                    local_results.extend(results);
                    local_results = Self::finalize_memory_results(local_results, MAX_SEARCH_RESULTS);
                }
                () = &mut deadline => {
                    break;
                }
            }
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
        anyhow::ensure!(
            timeout_secs <= MAX_SEARCH_SECONDS,
            "search timeout exceeds 30 seconds"
        );
        let mut filters = filters.clone();
        filters.room = Some(room_name.to_owned());
        let mut local_results = self.storage.search_skills(query, &filters, 50).await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

        let request_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<SkillSearchResult>>(32);

        let _registration = self.pending_skill_searches.register(
            request_id,
            room_name,
            SkillSearch {
                sender: tx,
                query: query.to_owned(),
                filters: filters.clone(),
            },
        )?;

        let search_msg = P2PMessage::new(P2PMessageBody::SkillSearchRequest {
            request_id,
            query: query.to_string(),
            filters: filters.clone(),
        });

        if !matches!(
            tokio::time::timeout_at(deadline, self.broadcast_to_room(room_name, search_msg)).await,
            Ok(Ok(()))
        ) {
            return Ok(local_results);
        }

        let deadline = tokio::time::sleep_until(deadline);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                Some(results) = rx.recv() => {
                    Self::merge_skill_results(&mut local_results, results);
                    local_results.sort_by_key(|r| (std::cmp::Reverse(r.rank), std::cmp::Reverse(r.entry.timestamp)));
                    local_results.truncate(MAX_SEARCH_RESULTS);
                }
                () = &mut deadline => {
                    break;
                }
            }
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
        anyhow::ensure!(
            timeout_secs > 0 && timeout_secs <= MAX_TASK_SECONDS,
            "task timeout must be 1..=300 seconds"
        );
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.into());
        let task_id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel::<TaskResult>();

        let _registration = self.task_waiters.register(task_id, room_name, tx)?;

        let now = now_unix();

        let msg = P2PMessage::new(P2PMessageBody::TaskRequest {
            task_id,
            source_peer: self.user_name.clone(),
            room: room_name.to_string(),
            description: description.to_string(),
            timeout_secs,
            timestamp: now,
        });

        tokio::time::timeout_at(deadline, self.broadcast_to_room(room_name, msg)).await??;

        let result = tokio::time::timeout_at(deadline, rx).await;

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

        tasks.retain(|t| now < t.timestamp.saturating_add(t.timeout_secs as u64));

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
        let notified = self.task_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let immediate = self.poll_tasks(room_filter).await;
        if !immediate.is_empty() {
            return immediate;
        }

        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs.min(MAX_SEARCH_SECONDS)),
            notified,
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

    pub async fn room_health(&self, room_name: &str) -> Option<RoomHealth> {
        let health = self.rooms.read().await.get(room_name)?.health.clone();
        Some(health.read().await.clone())
    }

    async fn supervise_room(
        &self,
        room: &str,
        bootstrap: Vec<iroh::EndpointId>,
        sender: GossipSender,
        receiver: GossipReceiver,
        health: Arc<RwLock<RoomHealth>>,
    ) {
        let mut known: HashSet<_> = bootstrap.into_iter().take(128).collect();
        let mut transport = Some((sender, receiver));
        let mut backoff = Backoff::default();
        loop {
            if let Some((sender, receiver)) = transport.take() {
                let started = Instant::now();
                let result = self
                    .receive_loop(room, &sender, receiver, &mut known, &health)
                    .await;
                if started.elapsed() >= Duration::from_secs(30) {
                    backoff.reset();
                }
                let error = result
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "room event stream closed".into());
                warn!(%room, %error, "reconnecting room subscription");
                let mut state = health.write().await;
                state.state = "reconnecting".into();
                state.neighbors = 0;
                state.last_error = Some(error);
            }
            // Release every sender for the failed subscription before creating another.
            if let Some(entry) = self.rooms.write().await.get_mut(room) {
                entry.sender = None;
            } else {
                return;
            }
            tokio::time::sleep(backoff.next()).await;
            health.write().await.reconnect_attempts += 1;
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                self.gossip
                    .subscribe(room_to_topic(room), known.iter().copied().collect()),
            )
            .await;
            match result {
                Ok(Ok(topic)) => {
                    let (sender, receiver) = topic.split();
                    if let Some(entry) = self.rooms.write().await.get_mut(room) {
                        entry.sender = Some(sender.clone());
                    } else {
                        return;
                    }
                    // Reset only after a useful connection, not a subscription that
                    // can immediately fail again. The inner loop tracks peer retries.
                    transport = Some((sender, receiver));
                }
                error => {
                    health.write().await.last_error = Some(format!("resubscribe failed: {error:?}"))
                }
            }
        }
    }

    async fn receive_loop(
        &self,
        room: &str,
        sender: &GossipSender,
        mut receiver: GossipReceiver,
        known: &mut HashSet<iroh::EndpointId>,
        health: &RwLock<RoomHealth>,
    ) -> Result<()> {
        use n0_future::TryStreamExt;
        let mut heartbeat = tokio::time::interval(self.presence.heartbeat);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut neighbors = HashSet::new();
        let mut retry = Backoff::default();
        let mut retry_at = Instant::now() + retry.next();
        loop {
            tokio::select! {
                event = receiver.try_next() => match event? {
                    Some(Event::Received(msg)) => self.handle_message(room, &msg.content).await,
                    Some(Event::NeighborUp(id)) => {
                        neighbors.insert(id);
                        if known.len() < 128 { known.insert(id); }
                        retry.reset();
                        let mut state = health.write().await;
                        state.state = "connected".into();
                        state.neighbors = neighbors.len();
                        state.last_error = None;
                        // Advertise immediately to newly reachable peers.
                        heartbeat.reset_immediately();
                    }
                    Some(Event::NeighborDown(id)) => {
                        neighbors.remove(&id);
                        let mut state = health.write().await;
                        state.neighbors = neighbors.len();
                        if neighbors.is_empty() {
                            state.state = "reconnecting".into();
                            state.last_error = Some("all direct neighbors disconnected".into());
                            retry.reset();
                            retry_at = Instant::now() + retry.next();
                        }
                    }
                    Some(Event::Lagged) => anyhow::bail!("gossip event stream lagged; resynchronizing presence"),
                    None => return Ok(()),
                },
                _ = heartbeat.tick() => {
                    self.broadcast_to_room(room, P2PMessage::new(P2PMessageBody::Join {
                        name: self.user_name.clone(), agent: self.agent_name.clone(),
                    })).await?;
                    // Keep offline peers for diagnostics, but bound their retention.
                    let retention = self.presence.offline_after.max(Duration::from_secs(3600));
                    if let Some(peers) = self.peers.write().await.get_mut(room) {
                        peers.retain(|_, p| p.last_seen.elapsed() < retention);
                    }
                }
                _ = tokio::time::sleep_until(retry_at), if neighbors.is_empty() && !known.is_empty() => {
                    health.write().await.reconnect_attempts += 1;
                    tokio::time::timeout(Duration::from_secs(5),
                        sender.join_peers(known.iter().copied().collect())).await??;
                    retry_at = Instant::now() + retry.next();
                }
            }
        }
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

        self.handle_verified_message(room_name, msg).await;
    }

    /// Ingress seam after envelope verification. Keeping this crate-visible
    /// lets tests exercise authorization, persistence, and conflict delivery
    /// without constructing a real gossip transport.
    pub(crate) async fn handle_verified_message(&self, room_name: &str, msg: P2PMessage) {
        let signed_by = msg.signed_by.clone();
        if signed_by.is_some()
            && let Some(peers) = self.peers.write().await.get_mut(room_name)
        {
            for peer in peers
                .values_mut()
                .filter(|p| p.accepts_identity(signed_by.as_ref()))
            {
                peer.last_seen = Instant::now();
            }
        }
        match msg.body {
            P2PMessageBody::Join { name, agent } => {
                let is_new = {
                    let mut peers = self.peers.write().await;
                    let room_peers = peers.entry(room_name.to_string()).or_default();
                    if let Some(existing) = room_peers.get(&name)
                        && !existing.accepts_identity(signed_by.as_ref())
                    {
                        warn!(room = %room_name, peer = %name, "rejected peer name takeover by a different identity");
                        return;
                    }
                    let is_new = !room_peers.contains_key(&name);
                    if let Some(peer) = room_peers.get_mut(&name) {
                        peer.agent = agent;
                        peer.last_seen = Instant::now();
                    } else {
                        room_peers.insert(name.clone(), PeerInfo::new(name, agent, signed_by));
                    }
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
                if let Some(room_peers) = peers.get_mut(room_name)
                    && peer_action_is_authenticated(room_peers, &name, signed_by.as_ref())
                {
                    room_peers.remove(&name);
                } else {
                    warn!(room = %room_name, peer = %name, "dropped leave whose peer identity does not match");
                }
            }
            P2PMessageBody::MemoryCreated { entry } => {
                if entry.room != room_name {
                    return;
                }
                if let Err(e) = self.storage.store(&entry).await {
                    warn!(error = %e, "failed to store received memory");
                }
            }
            P2PMessageBody::StatusUpdate { author, text } => {
                let mut peers = self.peers.write().await;
                if let Some(room_peers) = peers.get_mut(room_name)
                    && peer_action_is_authenticated(room_peers, &author, signed_by.as_ref())
                    && let Some(peer) = room_peers.get_mut(&author)
                {
                    peer.last_status = Some(text);
                    peer.last_seen = Instant::now();
                } else {
                    warn!(room = %room_name, peer = %author, "dropped status whose author identity does not match");
                }
            }
            P2PMessageBody::SearchRequest {
                request_id,
                query,
                filters,
            } => {
                let results = match self
                    .memory_results_for_peer(room_name, &query, filters)
                    .await
                {
                    Ok(results) => results,
                    Err(error) => {
                        warn!(%error, "memory search failed");
                        return;
                    }
                };
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
                if let Some(pending) = self.pending_searches.get(request_id, room_name) {
                    let results = results
                        .into_iter()
                        .filter(|entry| {
                            entry.matches_filters(&pending.filters)
                                && entry.matches_query(&pending.query)
                        })
                        .take(MAX_SEARCH_RESULTS)
                        .collect();
                    let _ = pending.sender.try_send(results);
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
                if room != room_name
                    || timeout_secs == 0
                    || timeout_secs > MAX_TASK_SECONDS
                    || !is_message_fresh(timestamp, now_unix())
                    || source_peer == self.user_name
                {
                    return;
                }
                info!(task_id = %task_id, from = %source_peer, "received delegated task");
                let mut tasks = self.incoming_tasks.lock().await;
                tasks.retain(|t| now_unix() < t.timestamp.saturating_add(t.timeout_secs.into()));
                if tasks.iter().any(|t| t.task_id == task_id) {
                    return;
                }
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
                if let Some(tx) = self.task_waiters.remove(task_id, room_name) {
                    let _ = tx.send(result);
                }
            }
            P2PMessageBody::SkillPublished { entry } => {
                if !self.validate_skill(room_name, &entry).await {
                    warn!(room = %room_name, "dropped invalid skill");
                    return;
                }
                if let Err(e) = self.storage.store_skill(&entry).await {
                    warn!(error = %e, "failed to store received skill");
                }
            }
            P2PMessageBody::SkillSearchRequest {
                request_id,
                query,
                filters,
            } => {
                let results = match self
                    .skill_results_for_peer(room_name, &query, filters)
                    .await
                {
                    Ok(results) => results,
                    Err(error) => {
                        warn!(%error, "skill search failed");
                        return;
                    }
                };
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
                if let Some(pending) = self.pending_skill_searches.get(request_id, room_name) {
                    let mut validated = Vec::new();
                    for mut result in results.into_iter().take(MAX_SEARCH_RESULTS) {
                        if result.entry.matches_filters(&pending.filters)
                            && result.entry.matches_query(&pending.query)
                            && self.validate_skill(room_name, &result.entry).await
                        {
                            // Peer-provided aggregate ranks have no proof. Only use local votes.
                            let Ok(rank) = self.storage.get_skill_rank(&result.entry.hash).await
                            else {
                                return;
                            };
                            result.rank = rank;
                            validated.push(result);
                        }
                    }
                    let _ = pending.sender.try_send(validated);
                }
            }
            P2PMessageBody::SkillVoteCast {
                skill_hash,
                voter,
                score,
            } => {
                let Some(identity) = signed_by else {
                    return;
                };
                if Some(&voter) != identity.voting_label().as_ref() {
                    return;
                }
                let Ok(Some(skill)) = self.storage.get_skill(&skill_hash).await else {
                    return;
                };
                if skill.room != room_name {
                    return;
                }
                let now = now_unix();
                let vote = SkillVote {
                    skill_hash,
                    voter,
                    score,
                    timestamp: now,
                };
                if let Err(e) = self.storage.vote_skill(&vote).await {
                    warn!(error = %e, "failed to store received skill vote");
                }
            }
            P2PMessageBody::FileActivity { entry } => {
                let author_is_authenticated = {
                    let peers = self.peers.read().await;
                    peers.get(room_name).is_some_and(|room_peers| {
                        peer_action_is_authenticated(room_peers, &entry.author, signed_by.as_ref())
                    })
                };
                if !author_is_authenticated {
                    warn!(room = %room_name, author = %entry.author, "dropped file activity whose author does not match the joined peer identity");
                    return;
                }
                if let Err(reason) = entry.validate_received(now_unix(), MAX_MESSAGE_AGE_SECS) {
                    warn!(room = %room_name, author = %entry.author, reason, "dropped invalid file activity");
                    return;
                }
                if let Err(e) = self.storage.store_file_activity(&entry, now_unix()).await {
                    warn!(error = %e, "failed to store received file activity");
                }
                if let Some(local) = self.dirty.get(&entry.repo, &entry.path) {
                    info!(repo = %entry.repo, path = %entry.path, peer = %entry.author, "conflicting file activity from peer");
                    let _ = self
                        .conflict_broadcast
                        .send(ConflictEvent { local, peer: entry });
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

    /// Merge validated results; ranks have already been recomputed from local votes.
    fn merge_skill_results(local: &mut Vec<SkillSearchResult>, incoming: Vec<SkillSearchResult>) {
        for result in incoming {
            if let Some(existing) = local.iter_mut().find(|r| r.entry.hash == result.entry.hash) {
                existing.rank = result.rank;
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
        match verify_signature(identity, &payload, signature).await {
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
    use crate::activity::FileActivityEntry;
    use crate::memory::MemoryKind;
    use crate::node::{BuddiesNode, BuddiesNodeConfig};

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

    fn activity(repo: &str, path: &str, author: &str, timestamp: u64) -> FileActivityEntry {
        FileActivityEntry {
            repo: repo.into(),
            branch: "main".into(),
            path: path.into(),
            kind: crate::activity::FileChangeKind::Changed,
            diff: "+line\n".into(),
            content_hash: "abc".into(),
            author: author.into(),
            timestamp,
        }
    }

    #[test]
    fn signed_peer_identity_cannot_impersonate_another_author() {
        let alice_identity = SignerIdentity::Gpg {
            key_id: "ALICE".into(),
        };
        let mallory_identity = SignerIdentity::Gpg {
            key_id: "MALLORY".into(),
        };
        let alice = PeerInfo::new("alice".into(), "codex".into(), Some(alice_identity.clone()));

        assert!(alice.accepts_identity(Some(&alice_identity)));
        assert!(!alice.accepts_identity(Some(&mallory_identity)));
        assert!(!alice.accepts_identity(None));
    }

    #[test]
    fn peer_actions_require_the_identity_that_joined_under_that_name() {
        let alice_identity = SignerIdentity::Gpg {
            key_id: "ALICE".into(),
        };
        let mallory_identity = SignerIdentity::Gpg {
            key_id: "MALLORY".into(),
        };
        let mut peers = HashMap::new();
        peers.insert(
            "alice".into(),
            PeerInfo::new("alice".into(), "codex".into(), Some(alice_identity.clone())),
        );

        assert!(peer_action_is_authenticated(
            &peers,
            "alice",
            Some(&alice_identity)
        ));
        assert!(!peer_action_is_authenticated(
            &peers,
            "alice",
            Some(&mallory_identity)
        ));
        assert!(!peer_action_is_authenticated(&peers, "mallory", None));
    }

    #[tokio::test]
    async fn verified_ingress_rejects_forgery_then_stores_and_reports_real_conflict() {
        let node = BuddiesNode::new(BuddiesNodeConfig {
            presence: crate::resilience::PresenceConfig::default(),
            user_name: "local".into(),
            agent_name: "codex".into(),
            data_dir: None,
            signer: None,
        })
        .await
        .expect("create in-memory node");
        let mut conflicts = node.subscribe_conflict_events();
        let now = now_unix();
        let local = activity("repo", "src/a.rs", "local", now);
        node.room_manager.dirty.update_repo(
            "repo",
            HashSet::from(["src/a.rs".to_string()]),
            [local.clone()],
        );

        let alice_identity = SignerIdentity::Gpg {
            key_id: "ALICE".into(),
        };
        let mallory_identity = SignerIdentity::Gpg {
            key_id: "MALLORY".into(),
        };
        let mut join = P2PMessage::new(P2PMessageBody::Join {
            name: "alice".into(),
            agent: "codex".into(),
        });
        join.signed_by = Some(alice_identity.clone());
        node.room_manager
            .handle_verified_message("room-a", join)
            .await;

        let peer = activity("repo", "src/a.rs", "alice", now);
        let mut forged = P2PMessage::new(P2PMessageBody::FileActivity {
            entry: peer.clone(),
        });
        forged.signed_by = Some(mallory_identity);
        node.room_manager
            .handle_verified_message("room-a", forged)
            .await;

        assert!(
            node.storage
                .get_file_activity("repo", None, now)
                .await
                .expect("read activity")
                .is_empty()
        );
        assert!(conflicts.try_recv().is_err());

        let mut authentic = P2PMessage::new(P2PMessageBody::FileActivity {
            entry: peer.clone(),
        });
        authentic.signed_by = Some(alice_identity);
        node.room_manager
            .handle_verified_message("room-a", authentic)
            .await;

        assert_eq!(
            node.storage
                .get_file_activity("repo", None, now)
                .await
                .expect("read activity"),
            vec![peer.clone()]
        );
        let conflict = conflicts.try_recv().expect("conflict event");
        assert_eq!(conflict.local, local);
        assert_eq!(conflict.peer, peer);

        node.shutdown().await.expect("shutdown node");
    }

    #[test]
    fn validate_file_activity_accepts_fresh_entry() {
        let now = 1_000_000;
        assert_eq!(
            activity("repo", "src/a.rs", "alice", now).validate_received(now, MAX_MESSAGE_AGE_SECS),
            Ok(())
        );
        // skew inside the freshness window is fine in both directions
        assert_eq!(
            activity("repo", "src/a.rs", "alice", now - MAX_MESSAGE_AGE_SECS)
                .validate_received(now, MAX_MESSAGE_AGE_SECS),
            Ok(())
        );
        assert_eq!(
            activity("repo", "src/a.rs", "alice", now + MAX_MESSAGE_AGE_SECS)
                .validate_received(now, MAX_MESSAGE_AGE_SECS),
            Ok(())
        );
    }

    #[test]
    fn validate_file_activity_rejects_stale_and_far_future_timestamps() {
        let now = 1_000_000;
        for bad in [
            now - MAX_MESSAGE_AGE_SECS - 1,
            now + MAX_MESSAGE_AGE_SECS + 1,
            0,
            u64::MAX, // would overflow TTL arithmetic if stored
        ] {
            assert!(
                activity("repo", "src/a.rs", "alice", bad)
                    .validate_received(now, MAX_MESSAGE_AGE_SECS)
                    .is_err(),
                "timestamp {bad} should be rejected"
            );
        }
    }

    #[test]
    fn validate_file_activity_rejects_nul_bytes_in_key_fields() {
        let now = 1_000_000;
        // NUL in repo/path/author would alias the redb key `repo\0path\0peer`
        for entry in [
            activity("re\u{0}po", "src/a.rs", "alice", now),
            activity("repo", "src/\u{0}a.rs", "alice", now),
            activity("repo", "src/a.rs", "al\u{0}ice", now),
        ] {
            assert_eq!(
                entry.validate_received(now, MAX_MESSAGE_AGE_SECS),
                Err("field contains NUL byte")
            );
        }
    }

    #[test]
    fn validate_file_activity_rejects_oversized_fields() {
        let now = 1_000_000;
        let big = "x".repeat(crate::activity::MAX_ACTIVITY_FIELD_BYTES + 1);
        for entry in [
            activity(&big, "src/a.rs", "alice", now),
            activity("repo", &big, "alice", now),
            activity("repo", "src/a.rs", &big, now),
        ] {
            assert_eq!(
                entry.validate_received(now, MAX_MESSAGE_AGE_SECS),
                Err("field exceeds length cap")
            );
        }
        // exactly at the cap passes
        let max = "x".repeat(crate::activity::MAX_ACTIVITY_FIELD_BYTES);
        assert_eq!(
            activity(&max, &max, &max, now).validate_received(now, MAX_MESSAGE_AGE_SECS),
            Ok(())
        );
    }

    #[test]
    fn validate_file_activity_rejects_oversized_branch() {
        let now = 1_000_000;
        let mut entry = activity("repo", "src/a.rs", "alice", now);
        entry.branch = "x".repeat(crate::activity::MAX_ACTIVITY_FIELD_BYTES + 1);
        assert_eq!(
            entry.validate_received(now, MAX_MESSAGE_AGE_SECS),
            Err("field exceeds length cap")
        );

        // exactly at the cap passes
        entry.branch = "x".repeat(crate::activity::MAX_ACTIVITY_FIELD_BYTES);
        assert_eq!(entry.validate_received(now, MAX_MESSAGE_AGE_SECS), Ok(()));
    }

    #[test]
    fn validate_file_activity_rejects_oversized_diff() {
        let now = 1_000_000;
        let max_diff_len =
            crate::activity::MAX_DIFF_BYTES + crate::activity::DIFF_TRUNCATION_MARKER.len();
        let mut entry = activity("repo", "src/a.rs", "alice", now);

        entry.diff = "x".repeat(max_diff_len + 1);
        assert_eq!(
            entry.validate_received(now, MAX_MESSAGE_AGE_SECS),
            Err("diff exceeds length cap")
        );

        // honest senders always truncate to at most this length; exactly at
        // the cap must be accepted
        entry.diff = "x".repeat(max_diff_len);
        assert_eq!(entry.validate_received(now, MAX_MESSAGE_AGE_SECS), Ok(()));
    }

    #[test]
    fn validate_file_activity_rejects_oversized_content_hash() {
        let now = 1_000_000;
        let mut entry = activity("repo", "src/a.rs", "alice", now);

        entry.content_hash = "x".repeat(crate::activity::MAX_CONTENT_HASH_BYTES + 1);
        assert_eq!(
            entry.validate_received(now, MAX_MESSAGE_AGE_SECS),
            Err("content_hash exceeds length cap")
        );

        // exactly at the cap passes (SHA-256 hex is 64 bytes, well under this)
        entry.content_hash = "x".repeat(crate::activity::MAX_CONTENT_HASH_BYTES);
        assert_eq!(entry.validate_received(now, MAX_MESSAGE_AGE_SECS), Ok(()));
    }

    #[test]
    fn merge_skill_results_uses_latest_locally_computed_rank() {
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
    async fn regression_node() -> BuddiesNode {
        BuddiesNode::new(BuddiesNodeConfig {
            presence: crate::resilience::PresenceConfig::default(),
            user_name: "local".into(),
            agent_name: "test".into(),
            data_dir: None,
            signer: None,
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn regression_rejects_cross_room_memory() {
        let node = regression_node().await;
        let entry = memory("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", now_unix());
        node.room_manager
            .handle_verified_message(
                "room-b",
                P2PMessage::new(P2PMessageBody::MemoryCreated {
                    entry: entry.clone(),
                }),
            )
            .await;
        assert!(node.storage.get(entry.id).await.unwrap().is_none());
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn regression_failed_broadcast_cleans_task_waiter() {
        let node = regression_node().await;
        assert!(
            node.room_manager
                .delegate_task("not-joined", "test", 1)
                .await
                .is_err()
        );
        assert!(node.room_manager.task_waiters.len() == 0);
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn regression_rejects_forged_voter() {
        let node = regression_node().await;
        let mut msg = P2PMessage::new(P2PMessageBody::SkillVoteCast {
            skill_hash: "hash".into(),
            voter: "victim".into(),
            score: 1,
        });
        msg.signed_by = Some(SignerIdentity::Gpg {
            key_id: "ATTACKER".into(),
        });
        node.room_manager
            .handle_verified_message("room-a", msg)
            .await;
        assert_eq!(node.storage.get_skill_rank("hash").await.unwrap(), 0);
        node.shutdown().await.unwrap();
    }
    fn valid_skill(title: &str, room: &str) -> SkillEntry {
        let mut entry = skill_result(title, 0).entry;
        entry.room = room.to_owned();
        entry.hash = crate::skill::skill_content_hash(&entry.title, &entry.content, &entry.tags);
        entry
    }

    #[tokio::test]
    async fn regression_peer_queries_cannot_read_other_rooms() {
        let node = regression_node().await;
        let entry = memory("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", 1);
        node.storage.store(&entry).await.unwrap();
        node.storage
            .store_skill(&valid_skill("secret", "room-a"))
            .await
            .unwrap();
        for room in [None, Some("room-a".to_owned())] {
            let memories = node
                .room_manager
                .memory_results_for_peer(
                    "room-b",
                    "",
                    SearchFilters {
                        room: room.clone(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let skills = node
                .room_manager
                .skill_results_for_peer(
                    "room-b",
                    "",
                    SkillSearchFilters {
                        room,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert!(memories.is_empty());
            assert!(skills.is_empty());
        }
        assert_eq!(
            node.room_manager
                .memory_results_for_peer("room-a", "", SearchFilters::default())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            node.room_manager
                .skill_results_for_peer("room-a", "", SkillSearchFilters::default())
                .await
                .unwrap()
                .len(),
            1
        );
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn regression_skill_responses_verify_content_signature_filters_and_rank() {
        let node = regression_node().await;
        let (_dir, signer) = crate::identity::test_signer().await;
        let identity = signer.identity();
        let mut valid = valid_skill("wanted", "room-a");
        valid.signature = Some(signer.sign(&valid.signing_payload()).await.unwrap());
        valid.signed_by = Some(identity.clone());
        node.room_manager
            .set_identity_policy("room-a", vec![identity.clone()], true)
            .await;
        node.storage.store_skill(&valid).await.unwrap();
        node.storage
            .vote_skill(&SkillVote {
                skill_hash: valid.hash.clone(),
                voter: identity.to_label(),
                score: -1,
                timestamp: 0,
            })
            .await
            .unwrap();
        let mut bad_hash = valid.clone();
        bad_hash.content = "forged".into();
        let mut bad_sig = valid.clone();
        bad_sig.signature = Some(vec![1, 2, 3]);
        let mut unsigned = valid.clone();
        unsigned.signature = None;
        unsigned.signed_by = None;
        let wrong_room = valid_skill("wanted", "room-b");
        let wrong_query = valid_skill("unrelated", "room-a");
        let results = [
            bad_hash,
            bad_sig,
            unsigned,
            wrong_room,
            wrong_query,
            valid.clone(),
        ]
        .into_iter()
        .map(|entry| SkillSearchResult {
            entry,
            rank: i64::MAX,
        })
        .collect::<Vec<_>>();
        let request_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let _registration = node
            .room_manager
            .pending_skill_searches
            .register(
                request_id,
                "room-a",
                SkillSearch {
                    sender: tx,
                    query: "wanted".into(),
                    filters: SkillSearchFilters {
                        room: Some("room-a".into()),
                        tags: None,
                    },
                },
            )
            .unwrap();
        let response = || {
            P2PMessage::new(P2PMessageBody::SkillSearchResponse {
                request_id,
                results: results.clone(),
                peer_name: "peer".into(),
            })
        };
        node.room_manager
            .handle_verified_message("room-b", response())
            .await;
        assert!(rx.try_recv().is_err());
        node.room_manager
            .handle_verified_message("room-a", response())
            .await;
        let received = rx.try_recv().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].entry.hash, valid.hash);
        assert_eq!(received[0].rank, -1);
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn regression_full_response_queue_does_not_block_ingress() {
        let node = regression_node().await;
        let request_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(vec![]).unwrap();
        let _registration = node
            .room_manager
            .pending_searches
            .register(
                request_id,
                "room-a",
                MemorySearch {
                    sender: tx,
                    query: "".into(),
                    filters: SearchFilters {
                        room: Some("room-a".into()),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        let msg = P2PMessage::new(P2PMessageBody::SearchResponse {
            request_id,
            results: vec![],
            peer_name: "peer".into(),
        });
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            node.room_manager.handle_verified_message("room-a", msg),
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_ok());
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn regression_cancelled_search_and_task_release_waiters() {
        let node = regression_node().await;
        node.room_manager.join_room("room-a", vec![]).await.unwrap();
        let manager = node.room_manager.clone();
        let search = tokio::spawn(async move {
            manager
                .search_distributed("room-a", "", &SearchFilters::default(), 30)
                .await
        });
        let manager = node.room_manager.clone();
        let skills = tokio::spawn(async move {
            manager
                .search_skills_distributed("room-a", "", &SkillSearchFilters::default(), 30)
                .await
        });
        let manager = node.room_manager.clone();
        let task = tokio::spawn(async move { manager.delegate_task("room-a", "task", 30).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while node.room_manager.pending_searches.len() == 0
                || node.room_manager.pending_skill_searches.len() == 0
                || node.room_manager.task_waiters.len() == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        search.abort();
        skills.abort();
        task.abort();
        let _ = search.await;
        let _ = skills.await;
        let _ = task.await;
        assert_eq!(node.room_manager.pending_searches.len(), 0);
        assert_eq!(node.room_manager.pending_skill_searches.len(), 0);
        assert_eq!(node.room_manager.task_waiters.len(), 0);
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn regression_rejects_cross_room_skills_tasks_and_task_responses() {
        let node = regression_node().await;
        let entry = valid_skill("secret", "room-a");
        node.room_manager
            .handle_verified_message(
                "room-b",
                P2PMessage::new(P2PMessageBody::SkillPublished {
                    entry: entry.clone(),
                }),
            )
            .await;
        assert!(node.storage.get_skill(&entry.hash).await.unwrap().is_none());
        let task_id = Uuid::new_v4();
        node.room_manager
            .handle_verified_message(
                "room-b",
                P2PMessage::new(P2PMessageBody::TaskRequest {
                    task_id,
                    source_peer: "peer".into(),
                    room: "room-a".into(),
                    description: "task".into(),
                    timeout_secs: 30,
                    timestamp: now_unix(),
                }),
            )
            .await;
        assert!(node.room_manager.poll_tasks(None).await.is_empty());
        let (tx, mut rx) = oneshot::channel();
        let _registration = node
            .room_manager
            .task_waiters
            .register(task_id, "room-a", tx)
            .unwrap();
        let response = || {
            P2PMessage::new(P2PMessageBody::TaskResponse {
                task_id,
                result: TaskResult::Error {
                    message: "result".into(),
                },
                completed_by: "peer".into(),
            })
        };
        node.room_manager
            .handle_verified_message("room-b", response())
            .await;
        assert!(rx.try_recv().is_err());
        node.room_manager
            .handle_verified_message("room-a", response())
            .await;
        assert!(rx.try_recv().is_ok());
        node.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn regression_authenticated_votes_are_one_per_identity_and_room() {
        let node = regression_node().await;
        let (_dir, signer) = crate::identity::test_signer().await;
        let identity = signer.identity();
        let entry = valid_skill("voted", "room-a");
        node.storage.store_skill(&entry).await.unwrap();
        let canonical = identity.voting_label().unwrap();
        for voter in ["forged".to_owned(), identity.to_label()] {
            if voter == canonical {
                continue;
            }
            let mut msg = P2PMessage::new(P2PMessageBody::SkillVoteCast {
                skill_hash: entry.hash.clone(),
                voter,
                score: 1,
            });
            msg.signed_by = Some(identity.clone());
            node.room_manager
                .handle_verified_message("room-a", msg)
                .await;
        }
        assert_eq!(node.storage.get_skill_rank(&entry.hash).await.unwrap(), 0);
        for (room, score, expected) in [
            ("room-b", 1, 0),
            ("room-a", 1, 1),
            ("room-a", 1, 1),
            ("room-a", -1, -1),
            ("room-a", 10, -1),
        ] {
            let mut msg = P2PMessage::new(P2PMessageBody::SkillVoteCast {
                skill_hash: entry.hash.clone(),
                voter: canonical.clone(),
                score,
            });
            msg.signed_by = Some(identity.clone());
            node.room_manager.handle_verified_message(room, msg).await;
            assert_eq!(
                node.storage.get_skill_rank(&entry.hash).await.unwrap(),
                expected
            );
        }
        node.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn heartbeat_recovers_presence_without_erasing_status_or_accepting_takeover() {
        let node = regression_node().await;
        let manager = &node.room_manager;
        let identity = SignerIdentity::Gpg {
            key_id: "a".repeat(40),
        };
        let mut peer = PeerInfo::new("alice".into(), "test".into(), Some(identity.clone()));
        peer.last_status = Some("working".into());
        peer.last_seen = Instant::now() - Duration::from_secs(100);
        manager
            .peers
            .write()
            .await
            .entry("a".into())
            .or_default()
            .insert("alice".into(), peer);
        let join = || {
            P2PMessage::new(P2PMessageBody::Join {
                name: "alice".into(),
                agent: "updated".into(),
            })
        };
        let mut forged = join();
        forged.signed_by = Some(SignerIdentity::Gpg {
            key_id: "b".repeat(40),
        });
        manager.handle_verified_message("a", forged).await;
        let peers = manager.get_room_peers("a").await;
        assert_eq!(peers["alice"].presence(manager.presence), Presence::Offline);
        let mut genuine = join();
        genuine.signed_by = Some(identity);
        manager.handle_verified_message("a", genuine).await;
        let peers = manager.get_room_peers("a").await;
        assert_eq!(peers["alice"].presence(manager.presence), Presence::Online);
        assert_eq!(peers["alice"].last_status.as_deref(), Some("working"));
        assert_eq!(peers["alice"].agent, "updated");
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_room_stream_retries_and_leave_cancels_supervision() {
        let node = regression_node().await;
        let manager = &node.room_manager;
        manager.join_room("failure", vec![]).await.unwrap();
        manager.gossip.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let health = manager.room_health("failure").await.unwrap();
                if health.reconnect_attempts > 0 && health.last_error.is_some() {
                    assert_eq!(health.state, "reconnecting");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let health = manager.rooms.read().await["failure"].health.clone();
        manager.leave_room("failure").await.unwrap();
        let retries = health.read().await.reconnect_attempts;
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(!manager.is_joined("failure").await);
        assert_eq!(health.read().await.reconnect_attempts, retries);
        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_joins_create_one_subscription() {
        let node = regression_node().await;
        let manager = &node.room_manager;
        let (a, b) = tokio::join!(
            manager.join_room("a", vec![]),
            manager.join_room("a", vec![])
        );
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(manager.list_rooms().await, vec!["a"]);
        node.shutdown().await.unwrap();
        assert!(manager.join_room("a", vec![]).await.is_err());
    }
    #[tokio::test]
    async fn p2p_presence_expires_and_recovers_after_unannounced_departure() {
        use iroh::{
            Endpoint, address_lookup::memory::MemoryLookup, endpoint::presets, protocol::Router,
        };
        let lookup = MemoryLookup::new();
        let a = Endpoint::builder(presets::Minimal)
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .address_lookup(lookup.clone())
            .bind()
            .await
            .unwrap();
        let b = Endpoint::builder(presets::Minimal)
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .address_lookup(lookup.clone())
            .bind()
            .await
            .unwrap();
        lookup.add_endpoint_info(a.addr());
        lookup.add_endpoint_info(b.addr());
        let ga = Gossip::builder().spawn(a.clone());
        let gb = Gossip::builder().spawn(b.clone());
        let ra = Router::builder(a.clone())
            .accept(iroh_gossip::ALPN, ga.clone())
            .spawn();
        let rb = Router::builder(b.clone())
            .accept(iroh_gossip::ALPN, gb.clone())
            .spawn();
        let config = PresenceConfig {
            heartbeat: Duration::from_millis(100),
            suspect_after: Duration::from_millis(400),
            offline_after: Duration::from_millis(800),
        };
        let storage = Arc::new(AsyncStorage::open(None).await.unwrap());
        let ma = RoomManager::new(
            ga,
            "alice".into(),
            "test".into(),
            storage.clone(),
            None,
            Arc::new(DirtySet::new()),
            config,
        );
        let mb = RoomManager::new(
            gb.clone(),
            "bob".into(),
            "test".into(),
            storage.clone(),
            None,
            Arc::new(DirtySet::new()),
            config,
        );
        ma.join_room("test", vec![]).await.unwrap();
        mb.join_room("test", vec![a.id()]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if ma.get_room_peers("test").await.contains_key("bob") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        // Stop receiving/sending without broadcasting Leave (crash-like behavior).
        mb.shutdown().await;
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                if ma.get_room_peers("test").await["bob"].presence(config) == Presence::Offline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let returned = RoomManager::new(
            gb,
            "bob".into(),
            "test".into(),
            storage,
            None,
            Arc::new(DirtySet::new()),
            config,
        );
        returned.join_room("test", vec![a.id()]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if ma.get_room_peers("test").await["bob"].presence(config) == Presence::Online {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(ma.room_health("test").await.unwrap().state, "connected");
        ma.shutdown().await;
        returned.shutdown().await;
        ra.shutdown().await.unwrap();
        rb.shutdown().await.unwrap();
    }
}
