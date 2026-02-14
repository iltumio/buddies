use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::memory::{MemoryEntry, MemoryKind, SearchFilters};
use crate::node::SmemoNode;
use crate::protocol::{P2PMessage, P2PMessageBody, TaskResult};
use crate::ticket::RoomTicket;

#[derive(Clone)]
pub struct SmemoServer {
    node: Arc<SmemoNode>,
    tool_router: ToolRouter<Self>,
}

impl SmemoServer {
    pub fn new(node: Arc<SmemoNode>) -> Self {
        Self {
            node,
            tool_router: Self::tool_router(),
        }
    }
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
    #[schemars(description = "Seconds to wait for tasks to arrive if none are pending (default 30, 0 = return immediately)")]
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
    let text = serde_json::to_string_pretty(v).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

fn err(msg: impl std::fmt::Display) -> McpError {
    McpError::invalid_params(msg.to_string(), None)
}

#[tool_router]
impl SmemoServer {
    #[tool(
        name = "join_room",
        description = "Join a named collaboration room. Optionally provide a ticket from another peer to bootstrap P2P connection. Returns a ticket that others can use to join."
    )]
    async fn join_room(&self, Parameters(req): Parameters<JoinRoomRequest>) -> Result<CallToolResult, McpError> {
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
    async fn leave_room(&self, Parameters(req): Parameters<LeaveRoomRequest>) -> Result<CallToolResult, McpError> {
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
    async fn store_memory(&self, Parameters(req): Parameters<StoreMemoryRequest>) -> Result<CallToolResult, McpError> {
        let kind: MemoryKind = req.kind.parse().map_err(|e: anyhow::Error| err(e.to_string()))?;

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

    #[tool(name = "list_rooms", description = "List all rooms you are currently in.")]
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
            TaskResult::Success { output } => {
                ok_json(&serde_json::json!({
                    "status": "completed",
                    "output": output,
                }))
            }
            TaskResult::Error { message } => {
                ok_json(&serde_json::json!({
                    "status": "error",
                    "error": message,
                }))
            }
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
            TaskResult::Error { message: req.output }
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
}

#[tool_handler]
impl ServerHandler for SmemoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "P2P shared memory server for collaborative AI agents. \
                 Join rooms to share memories and search across teammates' knowledge in real-time."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
