//! Bound abandoned HTTP sessions, including handles whose SDK worker has exited.
use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use rmcp::transport::streamable_http_server::{SessionId, session::local::LocalSessionManager};
use tokio::{sync::Mutex, time::Instant};

pub struct Sessions {
    pub manager: Arc<LocalSessionManager>,
    last_request: Mutex<HashMap<SessionId, Instant>>,
    idle: Duration,
}

impl Sessions {
    pub fn new(idle: Duration) -> Arc<Self> {
        let mut manager = LocalSessionManager::default();
        manager.session_config.keep_alive = Some(idle);
        Arc::new(Self {
            manager: Arc::new(manager),
            last_request: Mutex::new(HashMap::new()),
            idle,
        })
    }

    async fn touch(&self, id: &str) {
        // Always lock manager then activity; never record attacker-invented IDs.
        let sessions = self.manager.sessions.read().await;
        if let Some((id, _)) = sessions.get_key_value(id) {
            self.last_request
                .lock()
                .await
                .insert(id.clone(), Instant::now());
        }
    }

    pub async fn sweep(&self) {
        let expired = {
            let mut sessions = self.manager.sessions.write().await;
            let mut seen = self.last_request.lock().await;
            seen.retain(|id, _| sessions.contains_key(id));
            let now = Instant::now();
            // Also bound sessions that never completed initialization.
            for id in sessions.keys() {
                seen.entry(id.clone()).or_insert(now);
            }
            let expired: Vec<_> = seen
                .iter()
                .filter(|(_, t)| now.duration_since(**t) >= self.idle)
                .map(|(id, _)| id.clone())
                .collect();
            expired
                .into_iter()
                .filter_map(|id| {
                    seen.remove(&id);
                    sessions.remove(&id)
                })
                .collect::<Vec<_>>()
        };
        for handle in expired {
            // Removal from the manager makes subsequent requests return 404.
            let _ = tokio::time::timeout(Duration::from_secs(1), handle.close()).await;
        }
    }
}

pub async fn activity(
    State(sessions): State<Arc<Sessions>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(id) = request
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        sessions.touch(id).await;
    }
    let response = next.run(request).await;
    if let Some(id) = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        sessions.touch(id).await;
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::transport::streamable_http_server::SessionManager;

    #[tokio::test]
    async fn expires_abandoned_sessions_but_keeps_recent_activity() {
        let sessions = Sessions::new(Duration::from_secs(300));
        let (old, _old_transport) = sessions.manager.create_session().await.unwrap();
        let (live, _live_transport) = sessions.manager.create_session().await.unwrap();
        sessions.touch(&old).await;
        sessions.touch(&live).await;
        sessions
            .last_request
            .lock()
            .await
            .insert(old.clone(), Instant::now() - Duration::from_secs(301));
        sessions.touch("invented-session").await;
        assert_eq!(sessions.last_request.lock().await.len(), 2);
        sessions.sweep().await;
        assert!(!sessions.manager.has_session(&old).await.unwrap());
        assert!(sessions.manager.has_session(&live).await.unwrap());
        sessions.manager.close_session(&live).await.unwrap();
        sessions.sweep().await;
        assert!(sessions.last_request.lock().await.is_empty());
    }
}
