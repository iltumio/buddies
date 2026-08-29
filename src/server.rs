use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::activity::{ConflictEvent, diff_summary};
use crate::memory::{MemoryEntry, MemoryKind, SearchFilters};
use crate::node::BuddiesNode;
use crate::protocol::{P2PMessage, P2PMessageBody, SignerIdentity, TaskResult};
use crate::room::PendingTask;
use crate::skill::{SkillEntry, SkillSearchFilters, SkillVote, skill_content_hash};
use crate::ticket::RoomTicket;

#[derive(Clone)]
pub struct BuddiesServer {
    node: Arc<BuddiesNode>,
    tool_router: ToolRouter<Self>,
}

impl BuddiesServer {
    pub fn new(node: Arc<BuddiesNode>) -> Self {
        Self {
            node,
            tool_router: Self::tool_router(),
        }
    }
}

fn conflict_notification_payload(event: &ConflictEvent) -> serde_json::Value {
    let (peer_added, peer_removed) = diff_summary(&event.peer.diff);
    let (local_added, local_removed) = diff_summary(&event.local.diff);
    serde_json::json!({
        "repo": event.peer.repo,
        "path": event.peer.path,
        "peer": event.peer.author,
        "branch": event.peer.branch,
        "kind": event.peer.kind.to_string(),
        "timestamp": event.peer.timestamp,
        "lines_added": peer_added,
        "lines_removed": peer_removed,
        "local": {
            "peer": event.local.author,
            "branch": event.local.branch,
            "kind": event.local.kind.to_string(),
            "timestamp": event.local.timestamp,
            "lines_added": local_added,
            "lines_removed": local_removed,
        },
        "instructions": format!(
            "Peer '{}' changed '{}' (branch '{}'), which you have also modified locally. \
             Call get_peer_diff to inspect their change and reconcile before continuing.",
            event.peer.author, event.peer.path, event.peer.branch
        ),
    })
}

fn task_notification_payload(task: &PendingTask) -> serde_json::Value {
    let instructions = format!(
        "A peer agent has delegated a task to you. \
         Execute the task described in 'description' using the available tools, \
         then call 'submit_task_result' with: \
         task_id='{}', room='{}', source_peer='{}', success=true/false, and your output.",
        task.task_id, task.room, task.source_peer
    );
    serde_json::json!({
        "task_id": task.task_id.to_string(),
        "source_peer": task.source_peer,
        "room": task.room,
        "description": task.description,
        "timestamp": task.timestamp,
        "timeout_secs": task.timeout_secs,
        "instructions": instructions,
    })
}

fn spawn_notification_forwarder<T>(
    peer: rmcp::service::Peer<rmcp::RoleServer>,
    mut receiver: tokio::sync::broadcast::Receiver<T>,
    method: &'static str,
    payload: fn(&T) -> serde_json::Value,
) where
    T: Clone + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let notification = ServerNotification::CustomNotification(
                        CustomNotification::new(method, Some(payload(&event))),
                    );
                    if let Err(error) = peer.send_notification(notification).await {
                        tracing::warn!(%method, %error, "failed to forward notification");
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(%method, skipped, "notification listener lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::debug!(%method, "notification channel closed");
                    break;
                }
            }
        }
    });
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JoinRoomRequest {
    pub room: String,
    #[schemars(description = "Optional ticket string from another peer to bootstrap connection")]
    pub ticket: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LeaveRoomRequest {
    pub room: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StoreMemoryRequest {
    pub room: String,
    pub title: String,
    pub content: String,
    #[schemars(description = "One of: decision, implementation, context, skill, status")]
    pub kind: String,
    pub tags: Option<Vec<String>>,
    pub references: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchMemoryRequest {
    pub query: String,
    pub room: Option<String>,
    pub kind: Option<String>,
    pub tags: Option<Vec<String>>,
    #[schemars(description = "Seconds to wait for P2P responses (default 3)")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListMemoriesRequest {
    pub room: Option<String>,
    pub kind: Option<String>,
    pub tags: Option<Vec<String>>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NotifyPeersRequest {
    pub room: String,
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetRoomStatusRequest {
    pub room: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DelegateTaskRequest {
    pub room: String,
    #[schemars(description = "A clear description of the task for the remote agent to execute")]
    pub description: String,
    #[schemars(description = "Seconds to wait for a peer to complete the task (default 60)")]
    pub timeout_secs: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PollTasksRequest {
    pub room: Option<String>,
    #[schemars(
        description = "Seconds to wait for tasks to arrive if none are pending (default 30, 0 = return immediately)"
    )]
    pub wait_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubmitTaskResultRequest {
    pub task_id: String,
    pub room: String,
    pub source_peer: String,
    #[schemars(description = "true if the task was completed successfully")]
    pub success: bool,
    #[schemars(description = "The output (if success) or error message (if failure)")]
    pub output: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetIdentityPolicyRequest {
    pub room: String,
    #[schemars(description = "Allowed signer identities like gpg:<key_id> or ssh:<public_key>")]
    pub identities: Vec<String>,
    #[schemars(description = "If true, drop unsigned messages even when whitelist is empty")]
    pub require_signed: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddWhitelistedIdentityRequest {
    pub room: String,
    #[schemars(description = "Signer identity in form gpg:<key_id> or ssh:<public_key>")]
    pub identity: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetIdentityPolicyRequest {
    pub room: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PublishSkillRequest {
    pub room: String,
    pub title: String,
    pub content: String,
    pub tags: Option<Vec<String>>,
    pub version: Option<u32>,
    #[schemars(description = "Hash of the previous version of this skill, if updating")]
    pub parent_hash: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchSkillsRequest {
    pub query: String,
    pub room: Option<String>,
    pub tags: Option<Vec<String>>,
    #[schemars(description = "Seconds to wait for P2P responses (default 3)")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VoteSkillRequest {
    pub room: String,
    #[schemars(description = "Content hash of the skill to vote on")]
    pub hash: String,
    #[schemars(description = "1 for upvote, -1 for downvote")]
    pub score: i8,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSkillRequest {
    #[schemars(description = "Content hash of the skill to retrieve")]
    pub hash: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WatchRepoRequest {
    #[schemars(description = "Absolute path to a local git repository to watch")]
    pub repo_path: String,
    #[schemars(description = "Room to broadcast file activity to")]
    pub room: String,
    #[schemars(description = "Repo name agreed with teammates (default: directory name)")]
    pub repo_name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnwatchRepoRequest {
    pub repo_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckFileActivityRequest {
    #[schemars(description = "Repo name as agreed in watch_repo")]
    pub repo: String,
    #[schemars(description = "Repo-relative paths to check (default: all recent activity)")]
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPeerDiffRequest {
    pub repo: String,
    #[schemars(description = "Repo-relative file path")]
    pub path: String,
    #[schemars(description = "Peer (author) name whose diff to fetch")]
    pub peer: String,
}

#[derive(Debug, Serialize)]
struct MemoryOutput {
    id: String,
    author: String,
    room: String,
    kind: String,
    title: String,
    content: String,
    tags: Vec<String>,
    timestamp: u64,
}

#[derive(Debug, Serialize)]
struct SkillOutput {
    hash: String,
    author: String,
    room: String,
    title: String,
    content: String,
    tags: Vec<String>,
    version: u32,
    parent_hash: Option<String>,
    signed_by: Option<String>,
    timestamp: u64,
}

impl From<SkillEntry> for SkillOutput {
    fn from(e: SkillEntry) -> Self {
        Self {
            hash: e.hash,
            author: e.author,
            room: e.room,
            title: e.title,
            content: e.content,
            tags: e.tags,
            version: e.version,
            parent_hash: e.parent_hash,
            signed_by: e.signed_by.as_ref().map(|s| s.to_label()),
            timestamp: e.timestamp,
        }
    }
}

#[derive(Debug, Serialize)]
struct SkillSearchResultOutput {
    hash: String,
    author: String,
    room: String,
    title: String,
    content: String,
    tags: Vec<String>,
    version: u32,
    parent_hash: Option<String>,
    signed_by: Option<String>,
    timestamp: u64,
    rank: i64,
}

impl From<crate::skill::SkillSearchResult> for SkillSearchResultOutput {
    fn from(r: crate::skill::SkillSearchResult) -> Self {
        Self {
            hash: r.entry.hash,
            author: r.entry.author,
            room: r.entry.room,
            title: r.entry.title,
            content: r.entry.content,
            tags: r.entry.tags,
            version: r.entry.version,
            parent_hash: r.entry.parent_hash,
            signed_by: r.entry.signed_by.as_ref().map(|s| s.to_label()),
            timestamp: r.entry.timestamp,
            rank: r.rank,
        }
    }
}

impl From<MemoryEntry> for MemoryOutput {
    fn from(e: MemoryEntry) -> Self {
        Self {
            id: e.id.to_string(),
            author: e.author,
            room: e.room,
            kind: e.kind.to_string(),
            title: e.title,
            content: e.content,
            tags: e.tags,
            timestamp: e.timestamp,
        }
    }
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn ok_json<T: Serialize>(v: &T) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(v)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

fn err(msg: impl std::fmt::Display) -> McpError {
    McpError::invalid_params(msg.to_string(), None)
}

#[tool_router]
impl BuddiesServer {
    #[tool(
        name = "join_room",
        description = "Join a named collaboration room. Optionally provide a ticket from another peer to bootstrap P2P connection. Returns a ticket that others can use to join."
    )]
    async fn join_room(
        &self,
        Parameters(req): Parameters<JoinRoomRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut bootstrap_peers = vec![];

        if let Some(ref ticket_str) = req.ticket {
            let ticket: RoomTicket = ticket_str
                .parse()
                .map_err(|e: anyhow::Error| err(format!("invalid ticket: {e}")))?;
            bootstrap_peers = ticket.endpoints.iter().map(|e| e.id).collect();
        }

        let topic_id = self
            .node
            .room_manager
            .join_room(&req.room, bootstrap_peers)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let my_addr = self.node.endpoint.addr();
        let ticket = RoomTicket::new(req.room.clone(), topic_id, vec![my_addr]);

        let result = serde_json::json!({
            "room": req.room,
            "ticket": ticket.to_string(),
            "endpoint_id": self.node.endpoint.id().to_string(),
        });

        ok_json(&result)
    }

    #[tool(name = "leave_room", description = "Leave a collaboration room.")]
    async fn leave_room(
        &self,
        Parameters(req): Parameters<LeaveRoomRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.node
            .room_manager
            .leave_room(&req.room)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        ok_json(&serde_json::json!({ "left": req.room }))
    }

    #[tool(
        name = "store_memory",
        description = "Store a memory entry and broadcast it to all peers in the room. Use this to share decisions, implementation details, context, skills, or status updates."
    )]
    async fn store_memory(
        &self,
        Parameters(req): Parameters<StoreMemoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let kind: MemoryKind = req
            .kind
            .parse()
            .map_err(|e: anyhow::Error| err(e.to_string()))?;

        let refs: Vec<Uuid> = req
            .references
            .unwrap_or_default()
            .iter()
            .filter_map(|r| r.parse().ok())
            .collect();

        let entry = MemoryEntry {
            id: Uuid::new_v4(),
            author: self.node.endpoint.id().to_string(),
            timestamp: now_ts(),
            room: req.room.clone(),
            kind,
            title: req.title,
            content: req.content,
            tags: req.tags.unwrap_or_default(),
            references: refs,
        };

        self.node
            .storage
            .store(&entry)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let broadcast_msg = P2PMessage::new(P2PMessageBody::MemoryCreated {
            entry: entry.clone(),
        });
        let _ = self
            .node
            .room_manager
            .broadcast_to_room(&req.room, broadcast_msg)
            .await;

        let output: MemoryOutput = entry.into();
        ok_json(&output)
    }

    #[tool(
        name = "search_memory",
        description = "Search memories across your local store AND all peers in the room. Waits for P2P responses up to the timeout. Use this to find what teammates know about a topic."
    )]
    async fn search_memory(
        &self,
        Parameters(req): Parameters<SearchMemoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let filters = SearchFilters {
            room: req.room.clone(),
            kind: req.kind,
            tags: req.tags,
        };

        let timeout = req.timeout_secs.unwrap_or(3);

        let results = if let Some(ref room) = req.room {
            self.node
                .room_manager
                .search_distributed(room, &req.query, &filters, timeout)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        } else {
            self.node
                .storage
                .search(&req.query, &filters, 50)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };

        let outputs: Vec<MemoryOutput> = results.into_iter().map(Into::into).collect();
        ok_json(&outputs)
    }

    #[tool(
        name = "list_memories",
        description = "List memories from your local store, optionally filtered by room, kind, or tags."
    )]
    async fn list_memories(
        &self,
        Parameters(req): Parameters<ListMemoriesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let filters = SearchFilters {
            room: req.room,
            kind: req.kind,
            tags: req.tags,
        };
        let limit = req.limit.unwrap_or(20);

        let results = self
            .node
            .storage
            .list(&filters, limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let outputs: Vec<MemoryOutput> = results.into_iter().map(Into::into).collect();
        ok_json(&outputs)
    }

    #[tool(
        name = "notify_peers",
        description = "Broadcast a status update to all peers in a room. Use this to tell teammates what you're working on."
    )]
    async fn notify_peers(
        &self,
        Parameters(req): Parameters<NotifyPeersRequest>,
    ) -> Result<CallToolResult, McpError> {
        let msg = P2PMessage::new(P2PMessageBody::StatusUpdate {
            author: self.node.endpoint.id().to_string(),
            text: req.text.clone(),
        });

        self.node
            .room_manager
            .broadcast_to_room(&req.room, msg)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        ok_json(&serde_json::json!({
            "notified": req.room,
            "text": req.text,
        }))
    }

    #[tool(
        name = "get_room_status",
        description = "Get the list of peers in a room and their last known status."
    )]
    async fn get_room_status(
        &self,
        Parameters(req): Parameters<GetRoomStatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        let peers = self.node.room_manager.get_room_peers(&req.room).await;

        let peer_list: Vec<serde_json::Value> = peers
            .values()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "agent": p.agent,
                    "last_status": p.last_status,
                })
            })
            .collect();

        ok_json(&serde_json::json!({
            "room": req.room,
            "peers": peer_list,
        }))
    }

    #[tool(
        name = "list_rooms",
        description = "List all rooms you are currently in."
    )]
    async fn list_rooms(&self) -> Result<CallToolResult, McpError> {
        let rooms = self.node.room_manager.list_rooms().await;
        ok_json(&serde_json::json!({ "rooms": rooms }))
    }

    #[tool(
        name = "delegate_task",
        description = "Delegate a task to a peer agent in the room. Broadcasts the task and blocks until a peer completes it or the timeout expires. The result is returned as if executed locally."
    )]
    async fn delegate_task(
        &self,
        Parameters(req): Parameters<DelegateTaskRequest>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = req.timeout_secs.unwrap_or(60);

        let result = self
            .node
            .room_manager
            .delegate_task(&req.room, &req.description, timeout)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        match result {
            TaskResult::Success { output } => ok_json(&serde_json::json!({
                "status": "completed",
                "output": output,
            })),
            TaskResult::Error { message } => ok_json(&serde_json::json!({
                "status": "error",
                "error": message,
            })),
        }
    }

    #[tool(
        name = "poll_pending_tasks",
        description = "Check for tasks delegated to you by other agents in the room. Returns pending tasks that need your attention. Use wait_secs > 0 to long-poll (block until a task arrives or timeout)."
    )]
    async fn poll_pending_tasks(
        &self,
        Parameters(req): Parameters<PollTasksRequest>,
    ) -> Result<CallToolResult, McpError> {
        let wait = req.wait_secs.unwrap_or(30);
        let room_filter = req.room.as_deref();

        let tasks = if wait == 0 {
            self.node.room_manager.poll_tasks(room_filter).await
        } else {
            self.node
                .room_manager
                .wait_for_tasks(room_filter, wait)
                .await
        };

        let task_list: Vec<serde_json::Value> = tasks
            .iter()
            .map(|t| {
                serde_json::json!({
                    "task_id": t.task_id.to_string(),
                    "source_peer": t.source_peer,
                    "room": t.room,
                    "description": t.description,
                    "timeout_secs": t.timeout_secs,
                })
            })
            .collect();

        ok_json(&serde_json::json!({
            "tasks": task_list,
            "count": task_list.len(),
        }))
    }

    #[tool(
        name = "submit_task_result",
        description = "Submit the result of a delegated task back to the requesting agent. Call this after completing a task from poll_pending_tasks."
    )]
    async fn submit_task_result(
        &self,
        Parameters(req): Parameters<SubmitTaskResultRequest>,
    ) -> Result<CallToolResult, McpError> {
        let task_id: Uuid = req
            .task_id
            .parse()
            .map_err(|_| err("invalid task_id UUID"))?;

        let task = crate::room::PendingTask {
            task_id,
            source_peer: req.source_peer,
            room: req.room.clone(),
            description: String::new(),
            timestamp: now_ts(),
            timeout_secs: 0,
        };

        let result = if req.success {
            TaskResult::Success { output: req.output }
        } else {
            TaskResult::Error {
                message: req.output,
            }
        };

        self.node
            .room_manager
            .submit_task_result(&task, result)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        ok_json(&serde_json::json!({
            "submitted": true,
            "task_id": req.task_id,
        }))
    }

    #[tool(
        name = "set_identity_policy",
        description = "Set per-room identity whitelist and signature requirement. Only whitelisted signed messages are accepted when identities are configured."
    )]
    async fn set_identity_policy(
        &self,
        Parameters(req): Parameters<SetIdentityPolicyRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut parsed = Vec::with_capacity(req.identities.len());
        for identity in &req.identities {
            let id = SignerIdentity::parse(identity)
                .map_err(|e| err(format!("invalid identity '{identity}': {e}")))?;
            parsed.push(id);
        }
        let require_signed = req.require_signed.unwrap_or(false);

        self.node
            .room_manager
            .set_identity_policy(&req.room, parsed, require_signed)
            .await;

        let (identities, mode) = self.node.room_manager.get_identity_policy(&req.room).await;
        ok_json(&serde_json::json!({
            "room": req.room,
            "require_signed": mode,
            "identities": identities,
        }))
    }

    #[tool(
        name = "add_whitelisted_identity",
        description = "Add one allowed signer identity to a room policy. Identity format: gpg:<key_id> or ssh:<public_key>."
    )]
    async fn add_whitelisted_identity(
        &self,
        Parameters(req): Parameters<AddWhitelistedIdentityRequest>,
    ) -> Result<CallToolResult, McpError> {
        let identity = SignerIdentity::parse(&req.identity)
            .map_err(|e| err(format!("invalid identity: {e}")))?;
        self.node
            .room_manager
            .add_whitelisted_identity(&req.room, identity)
            .await;

        let (identities, mode) = self.node.room_manager.get_identity_policy(&req.room).await;
        ok_json(&serde_json::json!({
            "room": req.room,
            "require_signed": mode,
            "identities": identities,
        }))
    }

    #[tool(
        name = "get_identity_policy",
        description = "Get current signer identity policy for a room and local node identity loaded from git config."
    )]
    async fn get_identity_policy(
        &self,
        Parameters(req): Parameters<GetIdentityPolicyRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (identities, require_signed) =
            self.node.room_manager.get_identity_policy(&req.room).await;
        let local_identity = self.node.room_manager.signer_identity_label();
        ok_json(&serde_json::json!({
            "room": req.room,
            "require_signed": require_signed,
            "identities": identities,
            "local_identity": local_identity,
        }))
    }

    #[tool(
        name = "publish_skill",
        description = "Publish a content-addressable skill and broadcast it to all peers in the room. The skill is identified by a SHA-256 hash of its content, enabling automatic deduplication across peers. Returns the skill with its unique hash."
    )]
    async fn publish_skill(
        &self,
        Parameters(req): Parameters<PublishSkillRequest>,
    ) -> Result<CallToolResult, McpError> {
        let tags = req.tags.unwrap_or_default();
        let hash = skill_content_hash(&req.title, &req.content, &tags);

        let mut entry = SkillEntry {
            hash: hash.clone(),
            author: self.node.endpoint.id().to_string(),
            timestamp: now_ts(),
            room: req.room.clone(),
            title: req.title,
            content: req.content,
            tags,
            version: req.version.unwrap_or(1),
            parent_hash: req.parent_hash,
            signed_by: None,
            signature: None,
        };

        self.node.room_manager.try_sign_skill(&mut entry);

        self.node
            .storage
            .store_skill(&entry)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let broadcast_msg = P2PMessage::new(P2PMessageBody::SkillPublished {
            entry: entry.clone(),
        });
        let _ = self
            .node
            .room_manager
            .broadcast_to_room(&req.room, broadcast_msg)
            .await;

        let output: SkillOutput = entry.into();
        ok_json(&output)
    }

    #[tool(
        name = "search_skills",
        description = "Search skills across your local store AND all peers in the room. Results are ranked by votes (highest first). Use this to find the best skill for a task."
    )]
    async fn search_skills(
        &self,
        Parameters(req): Parameters<SearchSkillsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let filters = SkillSearchFilters {
            room: req.room.clone(),
            tags: req.tags,
        };

        let timeout = req.timeout_secs.unwrap_or(3);

        let results = if let Some(ref room) = req.room {
            self.node
                .room_manager
                .search_skills_distributed(room, &req.query, &filters, timeout)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        } else {
            self.node
                .storage
                .search_skills(&req.query, &filters, 50)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };

        let outputs: Vec<SkillSearchResultOutput> = results.into_iter().map(Into::into).collect();
        ok_json(&outputs)
    }

    #[tool(
        name = "vote_skill",
        description = "Upvote (+1) or downvote (-1) a skill by its content hash. Votes are broadcast to all peers in the room and affect search ranking."
    )]
    async fn vote_skill(
        &self,
        Parameters(req): Parameters<VoteSkillRequest>,
    ) -> Result<CallToolResult, McpError> {
        if req.score != 1 && req.score != -1 {
            return Err(err("score must be 1 (upvote) or -1 (downvote)"));
        }

        let voter = self.node.endpoint.id().to_string();

        let vote = SkillVote {
            skill_hash: req.hash.clone(),
            voter: voter.clone(),
            score: req.score,
            timestamp: now_ts(),
        };

        self.node
            .storage
            .vote_skill(&vote)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let broadcast_msg = P2PMessage::new(P2PMessageBody::SkillVoteCast {
            skill_hash: req.hash.clone(),
            voter,
            score: req.score,
        });
        let _ = self
            .node
            .room_manager
            .broadcast_to_room(&req.room, broadcast_msg)
            .await;

        let rank = self.node.storage.get_skill_rank(&req.hash).unwrap_or(0);

        ok_json(&serde_json::json!({
            "voted": true,
            "hash": req.hash,
            "your_score": req.score,
            "new_rank": rank,
        }))
    }

    #[tool(
        name = "get_skill",
        description = "Retrieve a specific skill by its content hash."
    )]
    async fn get_skill(
        &self,
        Parameters(req): Parameters<GetSkillRequest>,
    ) -> Result<CallToolResult, McpError> {
        let entry = self
            .node
            .storage
            .get_skill(&req.hash)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        match entry {
            Some(skill) => {
                let rank = self.node.storage.get_skill_rank(&req.hash).unwrap_or(0);
                let output = SkillSearchResultOutput::from(crate::skill::SkillSearchResult {
                    entry: skill,
                    rank,
                });
                ok_json(&output)
            }
            None => Err(err(format!("skill not found: {}", req.hash))),
        }
    }

    #[tool(
        name = "watch_repo",
        description = "Watch a local git repository and broadcast every uncommitted file change (with diff) to peers in the room. Enables check_file_activity, get_peer_diff, and proactive conflict notifications. WARNING: this shares source-code diffs with everyone in the room."
    )]
    async fn watch_repo(
        &self,
        Parameters(req): Parameters<WatchRepoRequest>,
    ) -> Result<CallToolResult, McpError> {
        let state = self
            .node
            .watcher_manager
            .watch(Path::new(&req.repo_path), &req.room, req.repo_name)
            .await
            .map_err(|e| err(e.to_string()))?;
        ok_json(&serde_json::json!({
            "watching": true,
            "repo": state.repo,
            "room": state.room,
        }))
    }

    #[tool(name = "unwatch_repo", description = "Stop watching a repository.")]
    async fn unwatch_repo(
        &self,
        Parameters(req): Parameters<UnwatchRepoRequest>,
    ) -> Result<CallToolResult, McpError> {
        let stopped = self
            .node
            .watcher_manager
            .unwatch(Path::new(&req.repo_path))
            .await
            .map_err(|e| err(e.to_string()))?;
        ok_json(&serde_json::json!({ "stopped": stopped }))
    }

    #[tool(
        name = "check_file_activity",
        description = "Check which peers recently changed files in a watched repo. Call this BEFORE editing files in a shared repo to avoid conflicts. Returns per-file: peer, branch, change kind, timestamp, and diff line counts."
    )]
    async fn check_file_activity(
        &self,
        Parameters(req): Parameters<CheckFileActivityRequest>,
    ) -> Result<CallToolResult, McpError> {
        let entries = self
            .node
            .storage
            .get_file_activity(&req.repo, req.paths.as_deref(), now_ts())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let activity: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                let (added, removed) = diff_summary(&e.diff);
                serde_json::json!({
                    "path": e.path,
                    "peer": e.author,
                    "kind": e.kind.to_string(),
                    "branch": e.branch,
                    "timestamp": e.timestamp,
                    "lines_added": added,
                    "lines_removed": removed,
                })
            })
            .collect();
        ok_json(&serde_json::json!({ "repo": req.repo, "activity": activity }))
    }

    #[tool(
        name = "get_peer_diff",
        description = "Get the full diff of a peer's latest change to a file in a watched repo."
    )]
    async fn get_peer_diff(
        &self,
        Parameters(req): Parameters<GetPeerDiffRequest>,
    ) -> Result<CallToolResult, McpError> {
        let entry = self
            .node
            .storage
            .get_peer_file_activity(&req.repo, &req.path, &req.peer, now_ts())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        match entry {
            Some(e) => ok_json(&serde_json::json!({
                "repo": e.repo,
                "path": e.path,
                "peer": e.author,
                "branch": e.branch,
                "kind": e.kind.to_string(),
                "timestamp": e.timestamp,
                "diff": e.diff,
            })),
            None => Err(err(format!(
                "no recent activity from '{}' on '{}' in repo '{}'",
                req.peer, req.path, req.repo
            ))),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BuddiesServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("P2P communication layer for AI agents. \
                 Join rooms to share knowledge, delegate tasks, and coordinate with other agents in real-time. \
                 When you receive a 'notifications/buddies/taskArrived' notification, you MUST: \
                 1) Execute the task described in the 'description' field using the available tools. \
                 2) Call 'submit_task_result' with the task_id, room, source_peer, success=true/false, and your output. \
                 This completes the task delegation loop so the requesting agent gets your result. \
                 To collaborate on a shared git repo, call 'watch_repo' with the repo path and room; \
                 then call 'check_file_activity' before editing files to see what peers changed, and \
                 'get_peer_diff' to read a peer's change. When you receive a \
                 'notifications/buddies/fileConflict' notification, inspect the peer's diff and \
                 reconcile with your local changes before continuing to edit that file.")
    }

    async fn on_initialized(&self, context: rmcp::service::NotificationContext<rmcp::RoleServer>) {
        spawn_notification_forwarder(
            context.peer.clone(),
            self.node.subscribe_task_events(),
            "notifications/buddies/taskArrived",
            task_notification_payload,
        );
        spawn_notification_forwarder(
            context.peer,
            self.node.subscribe_conflict_events(),
            "notifications/buddies/fileConflict",
            conflict_notification_payload,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{ConflictEvent, FileActivityEntry, FileChangeKind};

    fn activity(author: &str, branch: &str, diff: &str, timestamp: u64) -> FileActivityEntry {
        FileActivityEntry {
            repo: "repo".into(),
            branch: branch.into(),
            path: "src/lib.rs".into(),
            kind: FileChangeKind::Changed,
            diff: diff.into(),
            content_hash: "abc".into(),
            author: author.into(),
            timestamp,
        }
    }

    #[test]
    fn conflict_payload_contains_local_and_peer_metadata() {
        let event = ConflictEvent {
            local: activity("me", "feature/local", "+ours\n", 10),
            peer: activity("alice", "feature/peer", "+theirs\n-old\n", 20),
        };

        let payload = conflict_notification_payload(&event);

        assert_eq!(payload["peer"], "alice");
        assert_eq!(payload["branch"], "feature/peer");
        assert_eq!(payload["timestamp"], 20);
        assert_eq!(payload["lines_added"], 1);
        assert_eq!(payload["lines_removed"], 1);
        assert_eq!(payload["local"]["peer"], "me");
        assert_eq!(payload["local"]["branch"], "feature/local");
        assert_eq!(payload["local"]["timestamp"], 10);
        assert_eq!(payload["local"]["lines_added"], 1);
        assert_eq!(payload["local"]["lines_removed"], 0);
    }
}
