//! Durable queue for explicitly started automation workers. MCP sessions are not owners.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const LEASE_SECS: u64 = 30;
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn worker_id(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "worker:{}",
        data_encoding::HEXLOWER.encode(&Sha256::digest(secret.as_bytes()))
    )
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Outcome {
    pub success: bool,
    pub output: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub room: String,
    pub worker_id: String,
    pub description: String,
    pub state: String,
    pub created_at: u64,
    pub deadline: u64,
    pub lease_until: u64,
    pub claim: Option<Uuid>,
    pub outcome: Option<Outcome>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Worker {
    pub id: String,
    pub agent: String,
    pub room: String,
    pub runner: Uuid,
    pub last_seen: u64,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Queue {
    workers: BTreeMap<String, Worker>,
    jobs: BTreeMap<Uuid, Job>,
}
#[derive(Deserialize, Serialize)]
pub struct Request {
    pub secret: String,
    pub runner: Uuid,
    #[serde(flatten)]
    pub action: Action,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Register {
        room: String,
        agent: String,
    },
    Claim,
    Unregister,
    Heartbeat {
        task_id: Uuid,
        claim: Uuid,
    },
    Complete {
        task_id: Uuid,
        claim: Uuid,
        outcome: Outcome,
    },
}
impl Queue {
    fn expire(&mut self, now: u64) {
        for job in self.jobs.values_mut() {
            if job.outcome.is_none()
                && (now >= job.deadline || (job.state == "running" && now >= job.lease_until))
            {
                job.state = "failed".into();
                job.outcome = Some(Outcome {
                    success: false,
                    output: "Task expired or worker lost its lease; execution was not retried."
                        .into(),
                });
            }
        }
        self.jobs
            .retain(|_, j| j.outcome.is_none() || now.saturating_sub(j.deadline) < 7 * 86400);
    }
    pub fn workers(&mut self, now: u64) -> Vec<Worker> {
        self.expire(now);
        self.workers.values().cloned().collect()
    }
    pub fn enqueue(
        &mut self,
        room: &str,
        description: &str,
        target: Option<&str>,
        timeout: u32,
        now: u64,
    ) -> Result<Option<Job>> {
        self.expire(now);
        ensure!(
            (1..=300).contains(&timeout),
            "task timeout must be 1..=300 seconds"
        );
        ensure!(
            !description.trim().is_empty() && description.len() <= 64 * 1024,
            "task description must be 1..65536 bytes"
        );
        // Explicit targets may queue while offline. Automatic routing only chooses live workers.
        let chosen = self
            .workers
            .values()
            .filter(|w| {
                w.room == room
                    && target
                        .map(|t| t == w.id || t == w.agent)
                        .unwrap_or(now < w.last_seen + LEASE_SECS)
            })
            .min_by_key(|w| {
                (
                    self.jobs
                        .values()
                        .filter(|j| j.worker_id == w.id && j.outcome.is_none())
                        .count(),
                    &w.id,
                )
            });
        let Some(worker) = chosen else {
            ensure!(
                target.is_none(),
                "no registered worker matches target in this room"
            );
            return Ok(None);
        };
        ensure!(
            self.jobs.len() < 512,
            "worker task history is full; retention is seven days"
        );
        ensure!(
            self.jobs.values().filter(|j| j.outcome.is_none()).count() < 128,
            "worker queue is full"
        );
        let job = Job {
            id: Uuid::new_v4(),
            room: room.into(),
            worker_id: worker.id.clone(),
            description: description.into(),
            state: "queued".into(),
            created_at: now,
            deadline: now + u64::from(timeout),
            lease_until: 0,
            claim: None,
            outcome: None,
        };
        self.jobs.insert(job.id, job.clone());
        Ok(Some(job))
    }
    pub fn get(&mut self, room: &str, id: Uuid, now: u64) -> Result<Job> {
        self.expire(now);
        self.jobs
            .get(&id)
            .filter(|j| j.room == room)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("task not found in this room"))
    }
    pub fn apply(&mut self, req: Request, now: u64) -> Result<serde_json::Value> {
        ensure!(
            Uuid::parse_str(&req.secret).is_ok(),
            "invalid worker credential"
        );
        self.expire(now);
        let id = worker_id(&req.secret);
        if let Action::Register { room, agent } = req.action {
            ensure!(
                !room.trim().is_empty() && room.len() <= 256 && agent == "codex",
                "invalid room or worker agent"
            );
            if let Some(existing) = self.workers.get(&id) {
                ensure!(
                    existing.room == room && existing.agent == agent,
                    "worker identity belongs to another room or agent"
                );
                ensure!(
                    existing.runner == req.runner || now >= existing.last_seen + LEASE_SECS,
                    "worker identity is already active"
                );
            }
            ensure!(
                self.workers.contains_key(&id) || self.workers.len() < 128,
                "too many workers"
            );
            self.workers.insert(
                id.clone(),
                Worker {
                    id: id.clone(),
                    room,
                    agent,
                    runner: req.runner,
                    last_seen: now,
                },
            );
            return Ok(serde_json::json!({"worker_id":id}));
        }
        let worker = self
            .workers
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("register worker first"))?;
        ensure!(
            worker.runner == req.runner,
            "worker belongs to another running process"
        );
        worker.last_seen = now;
        match req.action {
            Action::Unregister => {
                worker.last_seen = 0;
                for job in self
                    .jobs
                    .values_mut()
                    .filter(|j| j.worker_id == id && j.state == "running")
                {
                    job.state = "failed".into();
                    job.outcome = Some(Outcome {
                        success: false,
                        output: "Worker stopped; uncertain execution was not retried".into(),
                    });
                }
                Ok(serde_json::json!({"worker_id":id,"status":"offline"}))
            }
            Action::Claim => {
                let active = self
                    .jobs
                    .values()
                    .find(|j| j.worker_id == id && j.state == "running")
                    .map(|j| j.id);
                let next = active.or_else(|| {
                    self.jobs
                        .values()
                        .filter(|j| j.worker_id == id && j.state == "queued")
                        .min_by_key(|j| (j.created_at, j.id))
                        .map(|j| j.id)
                });
                if let Some(next) = next {
                    let job = self.jobs.get_mut(&next).unwrap();
                    job.state = "running".into();
                    job.claim.get_or_insert_with(Uuid::new_v4);
                    job.lease_until = (now + LEASE_SECS).min(job.deadline);
                    Ok(serde_json::json!({"task":job}))
                } else {
                    Ok(serde_json::json!({"task":null}))
                }
            }
            Action::Heartbeat { task_id, claim } | Action::Complete { task_id, claim, .. } => {
                let job = self
                    .jobs
                    .get_mut(&task_id)
                    .ok_or_else(|| anyhow::anyhow!("unknown task"))?;
                ensure!(
                    job.worker_id == id && job.claim == Some(claim),
                    "task claim does not belong to this worker"
                );
                if job.outcome.is_some() {
                    return Ok(serde_json::json!({"task":job}));
                }
                match req.action {
                    Action::Complete { outcome, .. } => {
                        ensure!(
                            outcome.output.len() <= 256 * 1024,
                            "task output exceeds 256 KiB"
                        );
                        job.state = if outcome.success {
                            "completed"
                        } else {
                            "failed"
                        }
                        .into();
                        job.outcome = Some(outcome);
                    }
                    _ => {
                        job.lease_until = (now + LEASE_SECS).min(job.deadline);
                    }
                }
                Ok(serde_json::json!({"task":job}))
            }
            Action::Register { .. } => unreachable!(),
        }
    }
}

pub async fn handle(
    axum::extract::State(node): axum::extract::State<std::sync::Arc<crate::node::BuddiesNode>>,
    axum::Json(req): axum::Json<Request>,
) -> Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let result = async {
        // Join before registration so newly visible workers have a usable room.
        if let Action::Register { room, .. } = &req.action {
            node.join_room(room, None).await?;
        }
        node.storage
            .worker_queue(move |q| q.apply(req, now()))
            .await
    }
    .await;
    result
        .map(axum::Json)
        .map_err(|e: anyhow::Error| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    fn request(secret: &str, runner: Uuid, action: Action) -> Request {
        Request {
            secret: secret.into(),
            runner,
            action,
        }
    }
    fn register(q: &mut Queue, secret: &str, runner: Uuid, time: u64) {
        q.apply(
            request(
                secret,
                runner,
                Action::Register {
                    room: "room".into(),
                    agent: "codex".into(),
                },
            ),
            time,
        )
        .unwrap();
    }
    #[test]
    fn queue_survives_reopen_and_claims_are_exclusive_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let secret = Uuid::new_v4().to_string();
        let runner = Uuid::new_v4();
        let storage = Storage::open(&path).unwrap();
        let job = storage
            .worker_queue(|q| {
                register(q, &secret, runner, 100);
                q.enqueue("room", "work", None, 100, 100)
                    .map(|j| j.unwrap())
            })
            .unwrap();
        drop(storage);
        let storage = Storage::open(&path).unwrap();
        storage
            .worker_queue(|q| {
                assert_eq!(q.get("room", job.id, 101)?.state, "queued");
                assert!(q.get("other", job.id, 101).is_err());
                let a = q.apply(request(&secret, runner, Action::Claim), 101)?;
                let b = q.apply(request(&secret, runner, Action::Claim), 102)?;
                assert_eq!(a["task"]["claim"], b["task"]["claim"]);
                assert_eq!(a["task"]["id"], b["task"]["id"]);
                assert!(
                    q.apply(request(&secret, Uuid::new_v4(), Action::Claim), 102)
                        .is_err()
                );
                let claim = serde_json::from_value(a["task"]["claim"].clone())?;
                let outcome = Outcome {
                    success: true,
                    output: "done".into(),
                };
                q.apply(
                    request(
                        &secret,
                        runner,
                        Action::Complete {
                            task_id: job.id,
                            claim,
                            outcome: outcome.clone(),
                        },
                    ),
                    103,
                )?;
                // Late replies never replace a committed result.
                q.apply(
                    request(
                        &secret,
                        runner,
                        Action::Complete {
                            task_id: job.id,
                            claim,
                            outcome: Outcome {
                                success: false,
                                output: "wrong".into(),
                            },
                        },
                    ),
                    104,
                )?;
                assert_eq!(q.get("room", job.id, 104)?.outcome, Some(outcome));
                assert!(q.apply(request(&secret, runner, Action::Claim), 104)?["task"].is_null());
                Ok(())
            })
            .unwrap();
    }
    #[test]
    fn expired_worker_never_reexecutes_an_uncertain_task() {
        let mut q = Queue::default();
        let secret = Uuid::new_v4().to_string();
        let runner = Uuid::new_v4();
        register(&mut q, &secret, runner, 100);
        let job = q.enqueue("room", "work", None, 200, 100).unwrap().unwrap();
        q.apply(request(&secret, runner, Action::Claim), 101)
            .unwrap();
        let second = Uuid::new_v4();
        assert!(
            q.apply(
                request(
                    &secret,
                    second,
                    Action::Register {
                        room: "room".into(),
                        agent: "codex".into()
                    }
                ),
                110
            )
            .is_err()
        );
        register(&mut q, &secret, second, 132);
        assert_eq!(q.get("room", job.id, 132).unwrap().state, "failed");
        assert!(
            q.apply(request(&secret, second, Action::Claim), 132)
                .unwrap()["task"]
                .is_null()
        );
    }
    #[test]
    fn heartbeat_extends_lease_but_not_deadline_or_authority() {
        let mut q = Queue::default();
        let secret = Uuid::new_v4().to_string();
        let runner = Uuid::new_v4();
        register(&mut q, &secret, runner, 100);
        let job = q.enqueue("room", "work", None, 100, 100).unwrap().unwrap();
        let task = q
            .apply(request(&secret, runner, Action::Claim), 101)
            .unwrap();
        let claim = serde_json::from_value(task["task"]["claim"].clone()).unwrap();
        assert!(
            q.apply(
                request(
                    &secret,
                    runner,
                    Action::Heartbeat {
                        task_id: job.id,
                        claim: Uuid::new_v4()
                    }
                ),
                110
            )
            .is_err()
        );
        for time in [120, 140, 160, 180, 199] {
            q.apply(
                request(
                    &secret,
                    runner,
                    Action::Heartbeat {
                        task_id: job.id,
                        claim,
                    },
                ),
                time,
            )
            .unwrap();
            assert_eq!(q.get("room", job.id, time).unwrap().state, "running");
        }
        assert_eq!(q.get("room", job.id, 200).unwrap().state, "failed");
        assert!(q.enqueue("other", "work", Some("codex"), 100, 200).is_err());
    }
}
