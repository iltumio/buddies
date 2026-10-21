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

fn default_resource_name() -> String {
    "default".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResourceConfig {
    pub name: String,
    pub repo: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub authorized_paths: Vec<String>,
    #[serde(default)]
    pub checks: Vec<String>,
}

impl ResourceConfig {
    pub fn allows_path(&self, path: &str) -> bool {
        self.authorized_paths.is_empty()
            || self
                .authorized_paths
                .iter()
                .any(|p| p == "*" || p == "." || contains(p, path))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub room: String,
    pub repo: PathBuf,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resources: BTreeMap<String, ResourceConfig>,
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

impl Config {
    pub fn primary_resource_name(&self) -> String {
        if self.resources.is_empty() || self.resources.contains_key("default") {
            "default".into()
        } else {
            self.resources
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "default".into())
        }
    }

    pub fn all_resources(&self) -> BTreeMap<String, ResourceConfig> {
        if self.resources.is_empty() {
            let mut map = BTreeMap::new();
            map.insert(
                "default".into(),
                ResourceConfig {
                    name: "default".into(),
                    repo: self.repo.clone(),
                    description: None,
                    authorized_paths: self.authorized_paths.clone(),
                    checks: self.checks.clone(),
                },
            );
            map
        } else {
            self.resources.clone()
        }
    }

    pub fn resource(&self, name: Option<&str>) -> Result<ResourceConfig> {
        let all = self.all_resources();
        match name {
            Some(requested) => {
                if let Some(res) = all.get(requested) {
                    return Ok(res.clone());
                }
                if requested == "default" && all.len() == 1 {
                    return Ok(all.into_values().next().unwrap());
                }
                anyhow::bail!("unknown machine resource '{requested}'")
            }
            None => {
                if all.len() == 1 {
                    return Ok(all.into_values().next().unwrap());
                }
                if let Some(res) = all.get("default") {
                    return Ok(res.clone());
                }
                anyhow::bail!(
                    "multiple resources configured ({:?}); specify a resource name",
                    all.keys().collect::<Vec<_>>()
                )
            }
        }
    }
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
    #[serde(default = "default_resource_name")]
    pub resource: String,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Specialist {
    pub thread: Option<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub config: Config,
    pub base: String,
    pub integration_head: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resource_bases: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resource_heads: BTreeMap<String, String>,
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
    /// Create or update a specialist role and grant it access to machine resources and paths.
    CreateSpecialist {
        name: String,
        role: String,
        #[serde(default)]
        resources: Vec<String>,
        #[serde(default)]
        allowed_paths: Vec<String>,
    },
    Assign {
        id: String,
        owner: String,
        description: String,
        #[serde(default)]
        resource: Option<String>,
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
    #[cfg(test)]
    pub fn new(config: Config, base: String) -> Self {
        let mut bases = BTreeMap::new();
        bases.insert(config.primary_resource_name(), base);
        let specialists = ["specialist-a", "specialist-b"]
            .into_iter()
            .map(|s| (s.into(), Specialist::default()))
            .collect();
        Self::with_resources(config, bases, specialists)
    }

    pub fn with_resources(
        config: Config,
        bases: BTreeMap<String, String>,
        specialists: BTreeMap<String, Specialist>,
    ) -> Self {
        let primary = config.primary_resource_name();
        let base = bases
            .get(&primary)
            .or_else(|| bases.values().next())
            .cloned()
            .unwrap_or_default();
        let (resource_bases, resource_heads) = if config.resources.is_empty() && bases.len() <= 1 {
            (BTreeMap::new(), BTreeMap::new())
        } else {
            (bases.clone(), bases)
        };
        Self {
            version: 1,
            config,
            integration_head: base.clone(),
            base,
            resource_bases,
            resource_heads,
            coordinator_thread: None,
            specialists,
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

    pub fn resource_base(&self, resource: &str) -> String {
        self.resource_bases
            .get(resource)
            .cloned()
            .unwrap_or_else(|| self.base.clone())
    }

    pub fn resource_head(&self, resource: &str) -> String {
        self.resource_heads
            .get(resource)
            .cloned()
            .unwrap_or_else(|| self.integration_head.clone())
    }

    pub fn set_resource_head(&mut self, resource: &str, head: String) {
        if resource == self.config.primary_resource_name() {
            self.integration_head = head.clone();
        }
        if !self.resource_heads.is_empty() || !self.config.resources.is_empty() {
            self.resource_heads.insert(resource.into(), head);
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
            Control::CreateSpecialist {
                name,
                role,
                resources,
                allowed_paths,
            } => {
                ensure!(
                    actor == COORDINATOR,
                    "only orchestrator may create or configure specialists"
                );
                crate::conversation::validate_name(&name)?;
                ensure!(name != COORDINATOR, "orchestrator cannot be a specialist");
                text(&role)?;
                ensure!(role.len() <= 4096, "specialist role description too large");
                ensure!(
                    self.specialists.contains_key(&name) || self.specialists.len() < 16,
                    "specialist registry full; reuse existing roles"
                );
                ensure!(
                    !self.inflight.contains_key(&name) && self.current(&name).is_none(),
                    "cannot reconfigure an active specialist"
                );
                let granted_resources = if resources.is_empty() {
                    let default_res = self.config.resource(None)?;
                    vec![default_res.name]
                } else {
                    ensure!(resources.len() <= 16, "too many resources for specialist");
                    let mut list = Vec::new();
                    for r in resources {
                        let rc = self.config.resource(Some(&r))?;
                        if !list.contains(&rc.name) {
                            list.push(rc.name);
                        }
                    }
                    list
                };
                ensure!(
                    allowed_paths.len() <= 32,
                    "too many allowed paths for specialist"
                );
                for path in &allowed_paths {
                    validate_path(path)?;
                    ensure!(
                        granted_resources.iter().any(|r| {
                            self.config
                                .resource(Some(r))
                                .is_ok_and(|rc| rc.allows_path(path))
                        }),
                        "path '{}' outside granted resource scope",
                        path
                    );
                }
                let entry = self.specialists.entry(name.clone()).or_default();
                entry.role = Some(role);
                entry.resources = granted_resources.clone();
                entry.allowed_paths = allowed_paths;
                self.decision(format!(
                    "Configured specialist {name} with resources {granted_resources:?}"
                ));
            }
            Control::Assign {
                id,
                owner,
                description,
                resource,
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
                let target_res = if let Some(r) = resource.as_deref() {
                    self.config.resource(Some(r))?
                } else if let Some(s) = self
                    .specialists
                    .get(&owner)
                    .filter(|s| s.resources.len() == 1)
                {
                    self.config.resource(Some(&s.resources[0]))?
                } else {
                    self.config.resource(None)?
                };
                if let Some(s) = self.specialists.get(&owner) {
                    if !s.resources.is_empty() {
                        ensure!(
                            s.resources.contains(&target_res.name),
                            "specialist '{}' does not have access to resource '{}'; grant it with create_specialist",
                            owner,
                            target_res.name
                        );
                    }
                    if !s.allowed_paths.is_empty() {
                        for path in &paths {
                            ensure!(
                                s.allowed_paths
                                    .iter()
                                    .any(|allowed| contains(allowed, path)),
                                "path '{}' outside specialist '{}' granted allowed_paths",
                                path,
                                owner
                            );
                        }
                    }
                }
                for path in &paths {
                    validate_path(path)?;
                    ensure!(
                        target_res.allows_path(path),
                        "path outside authorized scope"
                    );
                    for a in self
                        .assignments
                        .values()
                        .filter(|a| a.state != Status::Cancelled && a.resource == target_res.name)
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
                let specialist = self.specialists.entry(owner.clone()).or_default();
                if !self.config.resources.is_empty()
                    && !specialist.resources.contains(&target_res.name)
                {
                    specialist.resources.push(target_res.name.clone());
                }
                self.assignments.insert(
                    id.clone(),
                    Assignment {
                        id,
                        owner,
                        revision: 1,
                        description,
                        resource: target_res.name,
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
    pub mirror_paths: Vec<PathBuf>,
}
impl Store {
    #[cfg(test)]
    pub fn open(path: PathBuf) -> Result<Self> {
        Self::open_with_mirrors(path, Vec::new())
    }
    pub fn open_with_mirrors(path: PathBuf, mirror_paths: Vec<PathBuf>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(&path)
            .context("open orchestrator state (another orchestrator may own this room)")?;
        let tx = db.begin_write()?;
        tx.open_table(TABLE)?;
        tx.commit()?;
        Ok(Self {
            db,
            path,
            mirror_paths,
        })
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
        for mirror in &self.mirror_paths {
            if let Some(parent) = mirror.parent() {
                let _ = std::fs::create_dir_all(parent);
                if let Ok(mut m) = tempfile::NamedTempFile::new_in(parent)
                    && std::io::Write::write_all(&mut m, value.as_bytes()).is_ok()
                {
                    let _ = m.as_file().sync_all();
                    let _ = m.persist(mirror);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub fn test_config(repo: PathBuf) -> Config {
    Config {
        room: "test".into(),
        repo,
        resources: BTreeMap::new(),
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
                resource: None,
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
    #[test]
    fn autonomous_specialist_creation_and_resource_access_enforcement() {
        let mut cfg = test_config(PathBuf::from("/backend"));
        cfg.resources.insert(
            "backend".into(),
            ResourceConfig {
                name: "backend".into(),
                repo: PathBuf::from("/backend"),
                description: Some("Backend API".into()),
                authorized_paths: vec!["src".into()],
                checks: vec!["true".into()],
            },
        );
        cfg.resources.insert(
            "frontend".into(),
            ResourceConfig {
                name: "frontend".into(),
                repo: PathBuf::from("/frontend"),
                description: Some("Frontend UI".into()),
                authorized_paths: vec!["src".into()],
                checks: vec!["true".into()],
            },
        );
        let mut bases = BTreeMap::new();
        bases.insert("backend".into(), "base-be".into());
        bases.insert("frontend".into(), "base-fe".into());
        let mut s = State::with_resources(cfg, bases, BTreeMap::new());

        // Coordinator creates two specialists with scoped resource access.
        s.apply(
            COORDINATOR,
            Control::CreateSpecialist {
                name: "api-specialist".into(),
                role: "Owns backend API".into(),
                resources: vec!["backend".into()],
                allowed_paths: vec!["src/api".into()],
            },
            1,
        )
        .unwrap();
        s.apply(
            COORDINATOR,
            Control::CreateSpecialist {
                name: "ui-specialist".into(),
                role: "Owns frontend UI".into(),
                resources: vec!["frontend".into()],
                allowed_paths: vec!["src/components".into()],
            },
            1,
        )
        .unwrap();

        // Cannot assign frontend resource to api-specialist (not granted).
        assert!(
            s.apply(
                COORDINATOR,
                Control::Assign {
                    id: "bad-resource".into(),
                    owner: "api-specialist".into(),
                    description: "Touch frontend".into(),
                    resource: Some("frontend".into()),
                    paths: vec!["src/api".into()],
                    dependencies: vec![],
                    deadline_secs: 60,
                },
                1,
            )
            .is_err()
        );

        // Cannot assign path outside specialist's allowed_paths.
        assert!(
            s.apply(
                COORDINATOR,
                Control::Assign {
                    id: "bad-path".into(),
                    owner: "api-specialist".into(),
                    description: "Touch db".into(),
                    resource: Some("backend".into()),
                    paths: vec!["src/db".into()],
                    dependencies: vec![],
                    deadline_secs: 60,
                },
                1,
            )
            .is_err()
        );

        // Valid assignments across different resources (even if relative paths would overlap, resources differ).
        s.apply(
            COORDINATOR,
            Control::Assign {
                id: "api-task".into(),
                owner: "api-specialist".into(),
                description: "Implement endpoint".into(),
                resource: None, // inferred from api-specialist's single granted resource
                paths: vec!["src/api/users.rs".into()],
                dependencies: vec![],
                deadline_secs: 60,
            },
            1,
        )
        .unwrap();
        assert_eq!(s.assignments["api-task"].resource, "backend");

        s.apply(
            COORDINATOR,
            Control::Assign {
                id: "ui-task".into(),
                owner: "ui-specialist".into(),
                description: "Consume endpoint".into(),
                resource: None, // inferred from ui-specialist's single granted resource
                paths: vec!["src/components/users.tsx".into()],
                dependencies: vec!["api-task".into()],
                deadline_secs: 60,
            },
            1,
        )
        .unwrap();
        assert_eq!(s.assignments["ui-task"].resource, "frontend");
    }
}
