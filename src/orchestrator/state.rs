//! The deterministic assignment graph. Model output is a proposal, never authority
//! to bypass revision, ownership, dependency or budget checks.
use anyhow::{Context, Result, ensure};
use redb::{Database, ReadableDatabase, TableDefinition};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

pub const COORDINATOR: &str = "orchestrator";
const TABLE: TableDefinition<&str, &str> = TableDefinition::new("orchestrator_state");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub room: String,
    pub repo: PathBuf,
    pub model: Option<String>,
    pub goal: String,
    pub authorized_paths: Vec<String>,
    pub checks: Vec<String>,
    pub max_active: usize,
    pub max_calls: u64,
    pub max_output_bytes: u64,
    pub max_elapsed_secs: u64,
    pub max_depth: usize,
    pub check_timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Running,
    WaitingForInput,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub revision: u64,
    pub summary: String,
    pub checks: Vec<String>,
    pub verified: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub id: String,
    pub owner: String,
    pub revision: u64,
    pub description: String,
    pub paths: Vec<String>,
    pub dependencies: Vec<String>,
    pub deadline: u64,
    pub timeout_secs: u64,
    pub last_heartbeat: u64,
    pub message_id: Option<uuid::Uuid>,
    pub depth: usize,
    pub state: Status,
    pub accepted_revision: Option<u64>,
    pub uncertain: bool,
    pub checkpoint: Option<String>,
    pub worktree: Option<PathBuf>,
    pub base: Option<String>,
    pub evidence: Vec<Evidence>,
    pub pending_result: Option<Evidence>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Specialist {
    pub thread: Option<String>,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub config: Config,
    pub base: String,
    pub integration_head: String,
    pub coordinator_thread: Option<String>,
    pub specialists: BTreeMap<String, Specialist>,
    pub assignments: BTreeMap<String, Assignment>,
    /// Written before dispatch. Restart never repeats an uncertain execution.
    pub inflight: BTreeMap<String, Vec<String>>,
    pub decisions: Vec<String>,
    pub calls: u64,
    pub output_bytes: u64,
    pub elapsed_ms: u64,
    pub completed: bool,
    pub final_summary: Option<String>,
    pub stopped_reason: Option<String>,
    pub publishing: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Inspect,
    Assign {
        id: String,
        owner: String,
        description: String,
        paths: Vec<String>,
        dependencies: Vec<String>,
        deadline_secs: u64,
    },
    Revise {
        id: String,
        description: String,
    },
    Cancel {
        id: String,
        reason: String,
    },
    /// Inspect all existing effects before explicitly authorizing another attempt.
    Recover {
        id: String,
        reviewed_effects: String,
        retry: bool,
    },
    Accept {
        id: String,
        revision: u64,
    },
    Checkpoint {
        id: String,
        revision: u64,
        detail: String,
    },
    Report {
        id: String,
        revision: u64,
        summary: String,
        checks: Vec<String>,
    },
    Fail {
        id: String,
        revision: u64,
        detail: String,
    },
    Heartbeat {
        id: String,
        revision: u64,
    },
    Complete {
        summary: String,
    },
}

pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 1024,
        "invalid ownership path"
    );
    ensure!(
        !path.contains(['\\', '\0', '\n', '\r']) && !path.starts_with('-'),
        "invalid ownership path"
    );
    ensure!(
        Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "paths must be normalized repository-relative paths (no dot, parent or absolute paths)"
    );
    ensure!(
        path.split('/')
            .all(|c| !c.is_empty() && c != ".git" && c != "."),
        "invalid ownership path"
    );
    Ok(())
}
pub fn contains(parent: &str, child: &str) -> bool {
    parent == child
        || child
            .strip_prefix(parent)
            .is_some_and(|tail| tail.starts_with('/'))
}
fn overlaps(a: &str, b: &str) -> bool {
    contains(a, b) || contains(b, a)
}
pub fn text(value: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 8192,
        "text must be 1..8192 bytes"
    );
    Ok(())
}

impl State {
    pub fn new(config: Config, base: String) -> Self {
        Self {
            version: 1,
            config,
            integration_head: base.clone(),
            base,
            coordinator_thread: None,
            specialists: ["specialist-a", "specialist-b"]
                .into_iter()
                .map(|s| (s.into(), Specialist::default()))
                .collect(),
            assignments: BTreeMap::new(),
            inflight: BTreeMap::new(),
            decisions: Vec::new(),
            calls: 0,
            output_bytes: 0,
            elapsed_ms: 0,
            completed: false,
            final_summary: None,
            stopped_reason: None,
            publishing: false,
        }
    }
    pub fn decision(&mut self, value: String) {
        // Bounded summaries; the full inbox/evidence remains in durable storage.
        if self.decisions.len() == 64 {
            self.decisions.remove(0);
        }
        self.decisions.push(value);
    }
    pub fn budget(&self) -> Result<()> {
        ensure!(
            self.calls < self.config.max_calls,
            "model call budget exhausted"
        );
        ensure!(
            self.output_bytes < self.config.max_output_bytes,
            "model output budget exhausted"
        );
        ensure!(
            self.elapsed_ms < self.config.max_elapsed_secs.saturating_mul(1000),
            "elapsed time budget exhausted"
        );
        Ok(())
    }
    pub fn reserve_call(&mut self) -> Result<()> {
        self.budget()?;
        self.calls += 1;
        Ok(())
    }
    pub fn account_output(&mut self, bytes: usize) -> Result<()> {
        self.output_bytes = self.output_bytes.saturating_add(bytes as u64);
        ensure!(
            self.output_bytes <= self.config.max_output_bytes,
            "model output budget exhausted"
        );
        Ok(())
    }
    pub fn current(&self, owner: &str) -> Option<&Assignment> {
        self.assignments.values().find(|a| {
            a.owner == owner && matches!(a.state, Status::Running | Status::WaitingForInput)
        })
    }
    pub fn ready(&self, now: u64) -> Vec<String> {
        let occupied = self
            .assignments
            .values()
            .filter(|a| a.state == Status::Running)
            .count();
        let mut owners = std::collections::BTreeSet::new();
        self.assignments
            .values()
            .filter(|a| {
                a.state == Status::Queued
                    && !a.uncertain
                    && a.deadline > now
                    && !self.inflight.contains_key(&a.owner)
                    && self.current(&a.owner).is_none()
                    && a.dependencies
                        .iter()
                        .all(|id| self.assignments[id].state == Status::Completed)
                    && owners.insert(a.owner.clone())
            })
            .take(self.config.max_active.saturating_sub(occupied))
            .map(|a| a.id.clone())
            .collect()
    }
    fn owned_mut(&mut self, actor: &str, id: &str, revision: u64) -> Result<&mut Assignment> {
        let a = self.assignments.get_mut(id).context("unknown assignment")?;
        ensure!(
            a.owner == actor,
            "only the assignment owner may accept or report"
        );
        ensure!(a.revision == revision, "stale assignment revision");
        ensure!(!a.uncertain, "inspect uncertain effects before resuming");
        ensure!(
            matches!(a.state, Status::Running | Status::WaitingForInput),
            "assignment is not dispatched"
        );
        Ok(a)
    }
    pub fn apply(&mut self, actor: &str, control: Control, now: u64) -> Result<()> {
        ensure!(!self.completed, "goal already completed");
        match control {
            Control::Inspect | Control::Complete { .. } => anyhow::bail!("runtime-only action"),
            Control::Assign {
                id,
                owner,
                description,
                paths,
                dependencies,
                deadline_secs,
            } => {
                ensure!(actor == COORDINATOR, "only orchestrator may assign work");
                crate::conversation::validate_name(&id)?;
                crate::conversation::validate_name(&owner)?;
                ensure!(
                    owner != COORDINATOR,
                    "orchestrator cannot own specialist work"
                );
                ensure!(
                    self.assignments.len() < 128 && !self.assignments.contains_key(&id),
                    "duplicate assignment or graph full"
                );
                text(&description)?;
                ensure!(
                    description.len() <= 4096
                        && paths.iter().map(String::len).sum::<usize>() <= 4096,
                    "assignment context too large"
                );
                ensure!(
                    !paths.is_empty() && paths.len() <= 32,
                    "explicit path ownership required"
                );
                for path in &paths {
                    validate_path(path)?;
                    ensure!(
                        self.config
                            .authorized_paths
                            .iter()
                            .any(|p| contains(p, path)),
                        "path outside authorized scope"
                    );
                    for a in self
                        .assignments
                        .values()
                        .filter(|a| a.state != Status::Cancelled)
                    {
                        ensure!(
                            !a.paths.iter().any(|p| overlaps(p, path)),
                            "shared contract already owned by {}",
                            a.id
                        );
                    }
                }
                ensure!(
                    deadline_secs > 0 && deadline_secs <= self.config.max_elapsed_secs,
                    "invalid assignment deadline"
                );
                ensure!(dependencies.len() <= 32, "too many dependencies");
                let mut depth = 0;
                for dep in &dependencies {
                    let a = self
                        .assignments
                        .get(dep)
                        .context("dependency must already exist (DAG)")?;
                    ensure!(a.state != Status::Cancelled, "dependency cancelled");
                    depth = depth.max(a.depth + 1);
                }
                ensure!(
                    depth <= self.config.max_depth,
                    "assignment depth budget exceeded"
                );
                ensure!(
                    self.specialists.contains_key(&owner) || self.specialists.len() < 16,
                    "specialist registry full; reuse existing roles"
                );
                self.specialists.entry(owner.clone()).or_default();
                self.assignments.insert(
                    id.clone(),
                    Assignment {
                        id,
                        owner,
                        revision: 1,
                        description,
                        paths,
                        dependencies,
                        deadline: now.saturating_add(deadline_secs),
                        timeout_secs: deadline_secs,
                        last_heartbeat: now,
                        message_id: None,
                        depth,
                        state: Status::Queued,
                        accepted_revision: None,
                        uncertain: false,
                        checkpoint: None,
                        worktree: None,
                        base: None,
                        evidence: Vec::new(),
                        pending_result: None,
                    },
                );
            }
            Control::Revise { id, description } => {
                ensure!(actor == COORDINATOR, "only orchestrator may revise");
                text(&description)?;
                ensure!(description.len() <= 4096, "assignment context too large");
                let mut affected = vec![id.clone()];
                ensure!(self.assignments.contains_key(&id), "unknown assignment");
                loop {
                    let more: Vec<_> = self
                        .assignments
                        .values()
                        .filter(|a| {
                            !affected.contains(&a.id)
                                && a.dependencies.iter().any(|d| affected.contains(d))
                        })
                        .map(|a| a.id.clone())
                        .collect();
                    if more.is_empty() {
                        break;
                    }
                    affected.extend(more);
                }
                // Validate the whole affected subgraph before changing any revision.
                for key in &affected {
                    let a = &self.assignments[key];
                    ensure!(a.revision < 128, "revision limit exceeded");
                    ensure!(
                        !a.uncertain && a.state != Status::Cancelled,
                        "cannot revise cancelled/uncertain assignment"
                    );
                    ensure!(
                        !self.inflight.contains_key(&a.owner),
                        "request a checkpoint and wait for the active turn to finish"
                    );
                    ensure!(
                        a.state != Status::Running
                            && (a.state != Status::WaitingForInput || a.checkpoint.is_some()),
                        "material scope change requires a specialist checkpoint"
                    );
                }
                for key in affected {
                    let a = self.assignments.get_mut(&key).unwrap();
                    a.revision += 1;
                    a.deadline = now.saturating_add(a.timeout_secs);
                    if key == id {
                        a.description = description.clone();
                    }
                    if let Some(e) = a.pending_result.take() {
                        a.evidence.push(e);
                    }
                    a.state = Status::Queued;
                    a.accepted_revision = None;
                    a.checkpoint = None;
                    // Keep prior worktree/base as inspectable evidence; dispatch creates a new one.
                }
            }
            Control::Cancel { id, reason } => {
                ensure!(actor == COORDINATOR, "only orchestrator may cancel");
                text(&reason)?;
                let a = self.assignments.get(&id).context("unknown assignment")?;
                ensure!(
                    a.state != Status::Completed,
                    "integrated work cannot be cancelled; revise to remove its owned effects"
                );
                ensure!(
                    !self.inflight.contains_key(&a.owner) && a.state != Status::Running,
                    "checkpoint before cancelling active work"
                );
                ensure!(
                    a.state != Status::WaitingForInput || a.checkpoint.is_some(),
                    "checkpoint before cancelling paused work"
                );
                ensure!(
                    !self
                        .assignments
                        .values()
                        .any(|a| a.state != Status::Cancelled && a.dependencies.contains(&id)),
                    "cancel dependents first"
                );
                self.assignments.get_mut(&id).unwrap().state = Status::Cancelled;
                self.decision(format!("Cancelled {id}: {reason}"));
            }
            Control::Recover {
                id,
                reviewed_effects,
                retry,
            } => {
                ensure!(actor == COORDINATOR, "only orchestrator may recover");
                text(&reviewed_effects)?;
                let a = self
                    .assignments
                    .get_mut(&id)
                    .context("unknown assignment")?;
                ensure!(
                    a.uncertain && !self.inflight.contains_key(&a.owner),
                    "assignment is not recoverable"
                );
                a.uncertain = false;
                if let Some(e) = a.pending_result.take() {
                    a.evidence.push(e);
                }
                a.state = if retry {
                    Status::Queued
                } else {
                    Status::Failed
                };
                if retry {
                    ensure!(a.revision < 128, "revision limit exceeded");
                    a.revision += 1;
                    a.accepted_revision = None;
                    a.deadline = now.saturating_add(a.timeout_secs);
                }
                self.decision(format!(
                    "Reviewed {id} effects; retry={retry}: {reviewed_effects}"
                ));
            }
            Control::Accept { id, revision } => {
                self.owned_mut(actor, &id, revision)?.accepted_revision = Some(revision);
            }
            Control::Checkpoint {
                id,
                revision,
                detail,
            } => {
                text(&detail)?;
                let a = self.owned_mut(actor, &id, revision)?;
                a.state = Status::WaitingForInput;
                a.checkpoint = Some(detail.clone());
                self.decision(format!("Checkpoint {id} revision {revision}: {detail}"));
            }
            Control::Report {
                id,
                revision,
                summary,
                checks,
            } => {
                text(&summary)?;
                ensure!(
                    !checks.is_empty() && checks.len() <= 8,
                    "report requires acceptance checks"
                );
                for check in &checks {
                    text(check)?;
                }
                let a = self
                    .assignments
                    .get_mut(&id)
                    .context("unknown assignment")?;
                ensure!(a.owner == actor, "only assignment owner may report");
                if revision < a.revision {
                    ensure!(a.evidence.len() < 128, "assignment evidence full");
                    a.evidence.push(Evidence {
                        revision,
                        summary,
                        checks,
                        verified: false,
                        detail: "Stale result retained; current revision is unsatisfied".into(),
                    });
                    return Ok(());
                }
                let a = self.owned_mut(actor, &id, revision)?;
                ensure!(
                    a.accepted_revision == Some(revision),
                    "accept the current revision before reporting"
                );
                ensure!(
                    a.pending_result.is_none() && a.evidence.len() < 128,
                    "result already pending or evidence full"
                );
                a.pending_result = Some(Evidence {
                    revision,
                    summary,
                    checks,
                    verified: false,
                    detail: "Awaiting turn completion and verification".into(),
                });
            }
            Control::Fail {
                id,
                revision,
                detail,
            } => {
                text(&detail)?;
                let a = self.owned_mut(actor, &id, revision)?;
                a.state = Status::Failed;
                self.decision(format!("Failed {id} revision {revision}: {detail}"));
            }
            Control::Heartbeat { id, revision } => {
                self.owned_mut(actor, &id, revision)?.last_heartbeat = now;
            }
        }
        Ok(())
    }
    pub fn recover_interrupted(&mut self) {
        let owners: Vec<_> = self.inflight.keys().cloned().collect();
        for owner in owners {
            for a in self.assignments.values_mut().filter(|a| {
                a.owner == owner
                    && a.worktree.is_some()
                    && a.state != Status::Completed
                    && a.state != Status::Cancelled
            }) {
                a.uncertain = true;
                a.state = Status::WaitingForInput;
            }
        }
        for a in self
            .assignments
            .values_mut()
            .filter(|a| a.state == Status::Running)
        {
            a.uncertain = true;
            a.state = Status::WaitingForInput;
        }
        if !self.inflight.is_empty() {
            self.decision("Execution interrupted. Inspect worktrees and evidence; no specialist work was replayed.".into());
        }
        self.inflight.clear();
    }
}

pub struct Store {
    db: Database,
    pub path: PathBuf,
}
impl Store {
    pub fn open(path: PathBuf) -> Result<Self> {
        let db = Database::create(&path)
            .context("open orchestrator state (another orchestrator may own this room)")?;
        let tx = db.begin_write()?;
        tx.open_table(TABLE)?;
        tx.commit()?;
        Ok(Self { db, path })
    }
    pub fn load(&self) -> Result<Option<State>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(TABLE)?;
        table
            .get("room")?
            .map(|v| serde_json::from_str(v.value()).map_err(Into::into))
            .transpose()
    }
    pub fn save(&self, state: &State) -> Result<()> {
        let value = serde_json::to_string_pretty(state)?;
        let tx = self.db.begin_write()?;
        tx.open_table(TABLE)?.insert("room", value.as_str())?;
        tx.commit()?;
        // A reviewable snapshot; redb remains the transactional source of truth.
        let mut file = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())?;
        std::io::Write::write_all(&mut file, value.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(self.path.with_extension("json"))?;
        Ok(())
    }
}

#[cfg(test)]
pub fn test_config(repo: PathBuf) -> Config {
    Config {
        room: "test".into(),
        repo,
        model: None,
        goal: "Producer/consumer".into(),
        authorized_paths: vec!["src".into()],
        checks: vec!["true".into()],
        max_active: 2,
        max_calls: 8,
        max_output_bytes: 10000,
        max_elapsed_secs: 120,
        max_depth: 2,
        check_timeout_secs: 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> State {
        State::new(test_config(PathBuf::from("/test")), "base".into())
    }
    fn assign(
        s: &mut State,
        id: &str,
        owner: &str,
        path: &str,
        dependencies: &[&str],
    ) -> Result<()> {
        s.apply(
            COORDINATOR,
            Control::Assign {
                id: id.into(),
                owner: owner.into(),
                description: id.into(),
                paths: vec![path.into()],
                dependencies: dependencies.iter().map(|d| (*d).into()).collect(),
                deadline_secs: 60,
            },
            1,
        )
    }
    fn report(s: &mut State, revision: u64) -> Result<()> {
        s.apply(
            "specialist-a",
            Control::Report {
                id: "producer".into(),
                revision,
                summary: "Implemented contract".into(),
                checks: vec!["true".into()],
            },
            2,
        )
    }
    #[test]
    fn revision_acceptance_stale_evidence_and_dependency_invalidation() {
        let mut s = state();
        assign(&mut s, "producer", "specialist-a", "src/producer", &[]).unwrap();
        assign(
            &mut s,
            "consumer",
            "specialist-b",
            "src/consumer",
            &["producer"],
        )
        .unwrap();
        assign(&mut s, "unaffected", "other", "src/other", &[]).unwrap();
        assert_eq!(s.ready(1), vec!["producer", "unaffected"]);
        s.assignments.get_mut("producer").unwrap().state = Status::Running;
        assert!(report(&mut s, 1).is_err());
        s.apply(
            "specialist-a",
            Control::Accept {
                id: "producer".into(),
                revision: 1,
            },
            2,
        )
        .unwrap();
        report(&mut s, 1).unwrap();
        // A report alone never releases dependent work.
        assert!(!s.ready(2).contains(&"consumer".into()));
        s.assignments.get_mut("producer").unwrap().state = Status::Completed;
        s.assignments.get_mut("consumer").unwrap().state = Status::Completed;
        s.assignments.get_mut("unaffected").unwrap().state = Status::Completed;
        s.apply(
            COORDINATOR,
            Control::Revise {
                id: "producer".into(),
                description: "Revised contract".into(),
            },
            3,
        )
        .unwrap();
        assert_eq!(s.assignments["producer"].revision, 2);
        assert_eq!(s.assignments["consumer"].revision, 2);
        assert_eq!(s.assignments["unaffected"].revision, 1);
        assert_eq!(s.assignments["unaffected"].state, Status::Completed);
        report(&mut s, 1).unwrap();
        assert_eq!(s.assignments["producer"].state, Status::Queued);
        assert!(
            s.assignments["producer"]
                .evidence
                .iter()
                .all(|e| !e.verified)
        );
        assert!(report(&mut s, 3).is_err());
        assert!(
            s.apply(
                "specialist-b",
                Control::Accept {
                    id: "producer".into(),
                    revision: 2
                },
                3
            )
            .is_err()
        );
    }
    #[test]
    fn material_changes_require_checkpoints_for_every_active_descendant() {
        let mut s = state();
        assign(&mut s, "producer", "specialist-a", "src/producer", &[]).unwrap();
        assign(
            &mut s,
            "consumer",
            "specialist-b",
            "src/consumer",
            &["producer"],
        )
        .unwrap();
        s.assignments.get_mut("producer").unwrap().state = Status::Completed;
        s.assignments.get_mut("consumer").unwrap().state = Status::Running;
        let revise = Control::Revise {
            id: "producer".into(),
            description: "New field".into(),
        };
        assert!(s.apply(COORDINATOR, revise.clone(), 3).is_err());
        assert_eq!(s.assignments["producer"].revision, 1);
        s.apply(
            "specialist-b",
            Control::Checkpoint {
                id: "consumer".into(),
                revision: 1,
                detail: "Current edits retained".into(),
            },
            3,
        )
        .unwrap();
        s.inflight.insert("specialist-b".into(), vec![]);
        assert!(s.apply(COORDINATOR, revise.clone(), 3).is_err());
        s.inflight.clear();
        s.apply(COORDINATOR, revise, 3).unwrap();
        assert_eq!(s.assignments["consumer"].accepted_revision, None);
        assert_eq!(s.assignments["consumer"].state, Status::Queued);
    }
    #[test]
    fn ownership_dag_concurrency_and_depth_are_enforced() {
        let mut s = state();
        s.config.max_active = 1;
        assign(&mut s, "producer", "specialist-a", "src/producer", &[]).unwrap();
        assert!(
            assign(
                &mut s,
                "overlap",
                "specialist-b",
                "src/producer/contract",
                &[]
            )
            .is_err()
        );
        assert!(assign(&mut s, "escape", "specialist-b", "outside", &[]).is_err());
        assert!(assign(&mut s, "cycle", "specialist-b", "src/cycle", &["cycle"]).is_err());
        assign(
            &mut s,
            "consumer",
            "specialist-b",
            "src/consumer",
            &["producer"],
        )
        .unwrap();
        assign(&mut s, "third", "third", "src/third", &["consumer"]).unwrap();
        assert!(assign(&mut s, "deep", "fourth", "src/deep", &["third"]).is_err());
        assign(&mut s, "independent", "third", "src/independent", &[]).unwrap();
        assert_eq!(s.ready(2).len(), 1);
        s.assignments.get_mut("independent").unwrap().state = Status::Running;
        assert!(s.ready(2).is_empty());
        for path in [
            ".",
            "../escape",
            "/absolute",
            "src/../escape",
            "src/.git/config",
            "src//file",
            "src/./file",
            "-option",
        ] {
            assert!(validate_path(path).is_err(), "{path}");
        }
        assert!(!contains("src/api", "src/apical"));
    }
    #[test]
    fn persistence_keeps_threads_receipts_budgets_and_uncertain_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.redb");
        let store = Store::open(path.clone()).unwrap();
        let mut s = state();
        assign(&mut s, "producer", "specialist-a", "src/producer", &[]).unwrap();
        s.assignments.get_mut("producer").unwrap().state = Status::Running;
        s.assignments.get_mut("producer").unwrap().worktree = Some(PathBuf::from("effects"));
        s.specialists.get_mut("specialist-a").unwrap().thread = Some("persistent-thread".into());
        s.inflight
            .insert("specialist-a".into(), vec!["message-id".into()]);
        s.reserve_call().unwrap();
        s.account_output(100).unwrap();
        s.elapsed_ms = 1000;
        store.save(&s).unwrap();
        assert!(Store::open(path.clone()).is_err());
        drop(store);
        let store = Store::open(path).unwrap();
        let mut s = store.load().unwrap().unwrap();
        s.recover_interrupted();
        assert_eq!(
            s.specialists["specialist-a"].thread.as_deref(),
            Some("persistent-thread")
        );
        assert_eq!((s.calls, s.output_bytes, s.elapsed_ms), (1, 100, 1000));
        assert!(s.assignments["producer"].uncertain);
        assert!(s.ready(3).is_empty());
        assert!(
            s.apply(
                "specialist-a",
                Control::Accept {
                    id: "producer".into(),
                    revision: 1
                },
                3
            )
            .is_err()
        );
        s.apply(
            COORDINATOR,
            Control::Recover {
                id: "producer".into(),
                reviewed_effects: "Inspected retained patch and file effects".into(),
                retry: true,
            },
            100,
        )
        .unwrap();
        assert_eq!(s.assignments["producer"].revision, 2);
        assert!(s.ready(101).contains(&"producer".into()));
    }
    #[test]
    fn budgets_survive_serialization_and_heartbeats_do_not_request_decisions() {
        let mut s = state();
        s.config.max_calls = 1;
        s.reserve_call().unwrap();
        assert!(s.reserve_call().is_err());
        assert!(s.account_output(10001).is_err());
        let mut s = state();
        assign(&mut s, "producer", "specialist-a", "src/producer", &[]).unwrap();
        s.assignments.get_mut("producer").unwrap().state = Status::Running;
        s.apply(
            "specialist-a",
            Control::Heartbeat {
                id: "producer".into(),
                revision: 1,
            },
            10,
        )
        .unwrap();
        assert!(s.decisions.is_empty());
        assert_eq!(s.assignments["producer"].last_heartbeat, 10);
    }
}
