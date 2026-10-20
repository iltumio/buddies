//! Generalized resource & connector isolation for specialist workers.
//!
//! A specialist worker can manage one or more resources (`git`, `linear`, `gdrive`, `fs`, etc.).
//! Each resource provides isolated per-assignment workspaces, scope ownership validation,
//! acceptance check execution, staged integration, and two-phase publication.
use super::{
    repo::{Repository, head},
    state::{Assignment, Config, Control, contains, validate_path},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

const MAX_CHECK_LOG: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResourceDescriptor {
    /// Logical resource identifier in the room, e.g. `git:backend`, `linear:roadmap`, `gdrive:specs`.
    pub id: String,
    /// Connector kind: `git`, `linear`, `gdrive`, `fs`, etc.
    pub kind: String,
    /// Authorized scopes within this resource (`["*"]` allows any valid relative path/item).
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Resource-level checks executed before final publication.
    #[serde(default)]
    pub checks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResourceClaim {
    pub resource: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "orchestrator_event", rename_all = "snake_case")]
pub enum WireEvent {
    Dispatch {
        assignment: Assignment,
        #[serde(default)]
        resource_heads: BTreeMap<String, String>,
    },
    Cancel {
        id: String,
        revision: u64,
    },
    PreparePublish {
        #[serde(default)]
        resource_heads: BTreeMap<String, String>,
    },
    Publish {
        #[serde(default)]
        resource_heads: BTreeMap<String, String>,
    },
    WorkerState {
        thread_id: Option<String>,
        active_assignment: Option<String>,
        accepted_revision: Option<u64>,
        inflight: bool,
        #[serde(default)]
        inflight_messages: Vec<String>,
        #[serde(default)]
        reserve_call: bool,
        #[serde(default)]
        output_bytes: usize,
        uncertain: bool,
        #[serde(default)]
        turn_completed: bool,
        #[serde(default)]
        turn_success: bool,
        #[serde(default)]
        turn_output: String,
        #[serde(default)]
        worktree: Option<PathBuf>,
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        resource_heads: BTreeMap<String, String>,
    },
    SpecialistControl {
        control: Control,
        #[serde(default)]
        worktree: Option<PathBuf>,
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        resource_heads: BTreeMap<String, String>,
    },
    VerificationResult {
        id: String,
        revision: u64,
        verified: bool,
        detail: String,
        #[serde(default)]
        integration_head: Option<String>,
        #[serde(default)]
        resource_heads: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone)]
pub struct ResourceSpec {
    pub descriptor: ResourceDescriptor,
    pub path: PathBuf,
}

impl ResourceSpec {
    /// Parse `--resource [<kind>:]<id>=<path>` or `--resource <path>`.
    pub fn parse(raw: &str) -> Result<Self> {
        let (left, right) = if let Some((l, r)) = raw.split_once('=') {
            (Some(l.trim()), r.trim())
        } else {
            (None, raw.trim())
        };
        ensure!(!right.is_empty(), "resource path must not be empty");
        let path = PathBuf::from(right)
            .canonicalize()
            .with_context(|| format!("canonicalize resource path '{right}'"))?;
        let is_git = path.join(".git").exists();
        let (kind, id) = match left {
            Some(label) if label.contains(':') => {
                let (k, name) = label.split_once(':').unwrap();
                ensure!(
                    !k.is_empty() && !name.is_empty(),
                    "invalid resource id '{label}'"
                );
                (k.to_owned(), label.to_owned())
            }
            Some(label) => {
                ensure!(!label.is_empty(), "resource id must not be empty");
                let k = if is_git { "git" } else { "fs" };
                (k.to_owned(), format!("{k}:{label}"))
            }
            None => {
                let k = if is_git { "git" } else { "fs" };
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("default");
                (k.to_owned(), format!("{k}:{name}"))
            }
        };
        Ok(Self {
            descriptor: ResourceDescriptor {
                id,
                kind,
                scopes: Vec::new(),
                checks: Vec::new(),
            },
            path,
        })
    }
    /// Parse `--specialist <name>=<boundary1>[,<boundary2>...]` into per-specialist `ResourceDescriptor` boundaries.
    pub fn parse_specialist_boundaries(
        specs: &[String],
        coordinator: &str,
        available_resources: &[ResourceDescriptor],
        default_checks: &[String],
    ) -> Result<BTreeMap<String, Vec<ResourceDescriptor>>> {
        let mut map: BTreeMap<String, BTreeMap<String, ResourceDescriptor>> = BTreeMap::new();
        for raw in specs {
            let (name_raw, rhs) = if let Some((n, b)) = raw.split_once('=') {
                (n.trim(), b.trim())
            } else if let Some((n, b)) = raw.split_once(':') {
                (n.trim(), b.trim())
            } else {
                anyhow::bail!(
                    "invalid --specialist '{raw}'; expected <name>=<boundary1>[,<boundary2>...]"
                );
            };
            crate::conversation::validate_name(name_raw)?;
            ensure!(
                name_raw != coordinator,
                "specialist name '{name_raw}' cannot equal orchestrator name '{coordinator}'"
            );
            let tokens: Vec<&str> = rhs
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .collect();
            ensure!(
                !tokens.is_empty(),
                "specialist '{name_raw}' must declare at least one boundary"
            );
            let spec_entry = map.entry(name_raw.to_owned()).or_default();
            for token in tokens {
                if !available_resources.is_empty() {
                    if let Some(res) = available_resources.iter().find(|r| {
                        r.id == token || r.id.split_once(':').map(|(_, short)| short) == Some(token)
                    }) {
                        let entry = spec_entry.entry(res.id.clone()).or_insert_with(|| {
                            ResourceDescriptor {
                                id: res.id.clone(),
                                kind: res.kind.clone(),
                                scopes: Vec::new(),
                                checks: res.checks.clone(),
                            }
                        });
                        for sc in &res.scopes {
                            if !entry.scopes.contains(sc) {
                                entry.scopes.push(sc.clone());
                            }
                        }
                        continue;
                    }
                    if let Some((res, scope)) = available_resources.iter().find_map(|r| {
                        token
                            .strip_prefix(&format!("{}:", r.id))
                            .or_else(|| {
                                r.id.split_once(':')
                                    .and_then(|(_, short)| token.strip_prefix(&format!("{short}:")))
                            })
                            .map(|rest| (r, rest))
                    }) {
                        if scope != "*" {
                            validate_path(scope)?;
                        }
                        ensure!(
                            res.scopes.iter().any(|s| contains(s, scope)),
                            "boundary scope '{scope}' for specialist '{name_raw}' is outside resource '{}' scopes",
                            res.id
                        );
                        let entry = spec_entry.entry(res.id.clone()).or_insert_with(|| {
                            ResourceDescriptor {
                                id: res.id.clone(),
                                kind: res.kind.clone(),
                                scopes: Vec::new(),
                                checks: res.checks.clone(),
                            }
                        });
                        if !entry.scopes.contains(&scope.to_owned()) {
                            entry.scopes.push(scope.to_owned());
                        }
                        continue;
                    }
                    if available_resources.len() == 1 {
                        let res = &available_resources[0];
                        if token != "*" {
                            validate_path(token)?;
                        }
                        ensure!(
                            res.scopes.iter().any(|s| contains(s, token)),
                            "boundary scope '{token}' for specialist '{name_raw}' is outside resource '{}' scopes",
                            res.id
                        );
                        let entry = spec_entry.entry(res.id.clone()).or_insert_with(|| {
                            ResourceDescriptor {
                                id: res.id.clone(),
                                kind: res.kind.clone(),
                                scopes: Vec::new(),
                                checks: res.checks.clone(),
                            }
                        });
                        if !entry.scopes.contains(&token.to_owned()) {
                            entry.scopes.push(token.to_owned());
                        }
                        continue;
                    }
                    anyhow::bail!(
                        "boundary '{token}' for specialist '{name_raw}' must reference a configured resource ({:?})",
                        available_resources
                            .iter()
                            .map(|r| &r.id)
                            .collect::<Vec<_>>()
                    );
                } else {
                    if token != "*" {
                        validate_path(token)?;
                    }
                    let entry =
                        spec_entry
                            .entry("default".into())
                            .or_insert_with(|| ResourceDescriptor {
                                id: "default".into(),
                                kind: "git".into(),
                                scopes: Vec::new(),
                                checks: default_checks.to_vec(),
                            });
                    if !entry.scopes.contains(&token.to_owned()) {
                        entry.scopes.push(token.to_owned());
                    }
                }
            }
        }
        ensure!(map.len() <= 32, "too many specialists (max 32)");
        Ok(map
            .into_iter()
            .map(|(name, res_map)| (name, res_map.into_values().collect()))
            .collect())
    }
}

/// Non-git stateful resource connector (`linear`, `gdrive`, `fs`, etc.).
///
/// Isolates each assignment revision in its own staging directory, validates
/// scope ownership on items/files, runs acceptance checks, stages verified
/// updates in `integration/`, and publishes with conflict preflight and rollback.
/// Publication across resources is not a distributed transaction.
pub struct StateConnector {
    pub id: String,
    pub kind: String,
    pub target: PathBuf,
    pub root: PathBuf,
    pub base_dir: PathBuf,
    pub integration: PathBuf,
    pub coordinator: PathBuf,
}

impl StateConnector {
    pub fn locate(target: &Path, room: &str, id: &str, kind: &str) -> Result<Self> {
        let target = target.canonicalize()?;
        ensure!(
            target.is_dir(),
            "connector target must be a directory: {}",
            target.display()
        );
        let key = format!("{room}:{id}");
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(key.as_bytes()));
        let root = target.join(".buddies-connector").join(&hash[..24]);
        let base_dir = root.join("base");
        let integration = root.join("integration");
        let coordinator = root.join("coordinator");
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            id: id.into(),
            kind: kind.into(),
            target,
            root,
            base_dir,
            integration,
            coordinator,
        })
    }

    pub fn initialize(&self, resume: bool) -> Result<(String, String)> {
        ensure!(
            !resume || (self.base_dir.exists() && self.integration.exists()),
            "incomplete saved state resource; inspect retained effects before recovery"
        );
        ensure!(
            resume || (!self.base_dir.exists() && !self.integration.exists()),
            "saved connector exists; use --resume"
        );
        if !resume || !self.base_dir.exists() || !self.integration.exists() {
            let target_files = collect_files(&self.target)?;
            let current_digest = digest_map(&target_files);
            if self.base_dir.exists() {
                let base_files = collect_files(&self.base_dir)?;
                ensure!(
                    digest_map(&base_files) == current_digest || !resume,
                    "connector target changed while resuming {}",
                    self.id
                );
            }
            sync_tree(&self.target, &self.base_dir)?;
            if !self.integration.exists() || !resume {
                sync_tree(&self.target, &self.integration)?;
            }
            sync_tree(&self.target, &self.coordinator)?;
        }
        let base = digest_map(&collect_files(&self.base_dir)?);
        let integ = digest_map(&collect_files(&self.integration)?);
        Ok((base, integ))
    }

    pub fn head(&self) -> Result<String> {
        Ok(digest_map(&collect_files(&self.integration)?))
    }

    pub fn workspace(&self, a: &Assignment) -> Result<(PathBuf, String)> {
        let head = self.head()?;
        let path = self
            .root
            .join(format!("{}-r{}-{}", a.id, a.revision, uuid::Uuid::new_v4()));
        sync_tree(&self.integration, &path)?;
        Ok((path, head))
    }

    pub async fn verify_assignment(
        &self,
        cwd: &Path,
        scopes: &[String],
        checks: &[String],
        timeout_secs: u64,
    ) -> Result<String> {
        let before = collect_files(&self.integration)?;
        let after = collect_files(cwd)?;
        let changed = changed_paths(&before, &after);
        for name in &changed {
            ensure!(
                scopes.iter().any(|s| s == "*" || contains(s, name)),
                "write outside resource '{}' assignment ownership: {name}",
                self.id
            );
        }
        run_checks(&self.root, cwd, checks, timeout_secs, &self.id, &self.kind).await?;
        let after = collect_files(cwd)?;
        for name in changed_paths(&before, &after) {
            ensure!(
                scopes.iter().any(|s| s == "*" || contains(s, &name)),
                "write outside resource '{}' assignment ownership: {name}",
                self.id
            );
        }
        sync_tree(cwd, &self.integration)?;
        let head = self.head()?;
        std::fs::write(self.root.join("integration_head"), &head)?;
        Ok(head)
    }

    pub async fn prepare_publication(
        &self,
        authorized_scopes: &[String],
        checks: &[String],
        timeout_secs: u64,
    ) -> Result<()> {
        let prepared = self.root.join("prepared.json");
        if prepared.exists() {
            std::fs::remove_file(prepared)?;
        }
        run_checks(
            &self.root,
            &self.integration,
            checks,
            timeout_secs,
            &self.id,
            &self.kind,
        )
        .await?;
        let base = collect_files(&self.base_dir)?;
        let integ = collect_files(&self.integration)?;
        let live = collect_files(&self.target)?;
        for name in changed_paths(&base, &integ) {
            ensure!(
                authorized_scopes
                    .iter()
                    .any(|s| s == "*" || contains(s, &name)),
                "change outside authorized resource scopes on '{}': {name}",
                self.id
            );
            ensure!(
                live.get(&name) == base.get(&name) || live.get(&name) == integ.get(&name),
                "target resource '{}' was modified externally on '{name}'",
                self.id
            );
        }
        std::fs::write(
            self.root.join("prepared.json"),
            serde_json::to_vec(&(digest_map(&base), digest_map(&integ)))?,
        )?;
        Ok(())
    }

    pub fn publish(&self) -> Result<()> {
        ensure!(
            !self.root.join("publication.json").exists(),
            "publication outcome uncertain; inspect publication.json before retrying"
        );
        let base = collect_files(&self.base_dir)?;
        let integ = collect_files(&self.integration)?;
        let prepared: (String, String) = serde_json::from_slice(
            &std::fs::read(self.root.join("prepared.json")).context("prepare publication first")?,
        )?;
        ensure!(
            prepared == (digest_map(&base), digest_map(&integ)),
            "resource changed after publication was prepared"
        );
        let live = collect_files(&self.target)?;
        let changed = changed_paths(&base, &integ);
        // Check the entire change set before touching any target file.
        for name in &changed {
            ensure!(
                live.get(name) == base.get(name) || live.get(name) == integ.get(name),
                "target resource '{}' changed before publication on '{name}'",
                self.id
            );
            if integ.contains_key(name) {
                ensure!(
                    live.keys().all(|other| changed.contains(other)
                        || !(other.starts_with(&format!("{name}/"))
                            || name.starts_with(&format!("{other}/")))),
                    "target path conflicts with an external file: {name}"
                );
            }
        }
        // Keep a durable recovery record until both target and baseline agree.
        // A process crash is deliberately not retried automatically.
        let journal = self.root.join("publication.json");
        std::fs::write(&journal, serde_json::to_vec(&live)?)?;
        if let Err(error) = apply_file_changes(&self.target, &changed, &integ) {
            match apply_file_changes(&self.target, &changed, &live) {
                Ok(()) => {
                    std::fs::remove_file(&journal)?;
                }
                Err(rollback) => {
                    return Err(error.context(format!(
                        "rollback failed ({rollback}); retained recovery snapshot at {}",
                        journal.display()
                    )));
                }
            }
            return Err(error.context("publication failed; target changes rolled back"));
        }
        sync_tree(&self.target, &self.base_dir)?;
        std::fs::remove_file(journal)?;
        std::fs::remove_file(self.root.join("prepared.json"))?;
        Ok(())
    }
}

pub enum ConnectorEngine {
    Git {
        repo: Repository,
        base_head: String,
        integration_head: String,
        prepared_patch: Option<Vec<u8>>,
    },
    State {
        connector: StateConnector,
        integration_head: String,
    },
}

pub struct ManagedResource {
    pub descriptor: ResourceDescriptor,
    pub engine: ConnectorEngine,
    // Held for the entire runtime, including resume and publication.
    _lock: std::fs::File,
}

impl ManagedResource {
    pub async fn paths(spec: &ResourceSpec, room: &str) -> Result<(PathBuf, PathBuf)> {
        if spec.descriptor.kind == "git" {
            let repo =
                Repository::locate(&spec.path, &format!("{room}:{}", spec.descriptor.id)).await?;
            Ok((repo.root, repo.coordinator))
        } else {
            let connector = StateConnector::locate(
                &spec.path,
                room,
                &spec.descriptor.id,
                &spec.descriptor.kind,
            )?;
            Ok((connector.root, connector.coordinator))
        }
    }

    pub async fn open(spec: ResourceSpec, room: &str, resume: bool) -> Result<Self> {
        let (root, _) = Self::paths(&spec, room).await?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("resource.lock"))?;
        lock.try_lock()
            .context("resource already in use by another worker/orchestrator")?;
        let initialized = root.join("base_head").exists()
            || root.join("base").exists()
            || root.join("integration").exists();
        ensure!(!resume || initialized, "no saved resource to resume");
        ensure!(
            resume || !initialized,
            "saved resource exists; use --resume; retained effects were not changed"
        );
        ensure!(
            !root.join("publication.json").exists(),
            "publication outcome uncertain; inspect publication.json before resuming"
        );
        let mut descriptor = spec.descriptor;
        if descriptor.scopes.is_empty() {
            descriptor.scopes.push("*".into());
        }
        let descriptor_file = root.join("resource.json");
        if descriptor_file.exists() {
            let saved: ResourceDescriptor =
                serde_json::from_slice(&std::fs::read(&descriptor_file)?)?;
            ensure!(saved == descriptor, "saved resource scopes/checks differ");
        }
        std::fs::write(&descriptor_file, serde_json::to_vec_pretty(&descriptor)?)?;
        if descriptor.kind == "git" {
            let repo = Repository::locate(&spec.path, &format!("{room}:{}", descriptor.id)).await?;
            let base_file = repo.root.join("base_head");
            let integ_file = repo.root.join("integration_head");
            ensure!(
                !resume || (base_file.exists() && integ_file.exists()),
                "incomplete saved Git resource; inspect retained effects before recovery"
            );
            let current = head(&repo.repo).await?;
            let (base_head, integration_head) =
                if resume && base_file.exists() && integ_file.exists() {
                    let b = std::fs::read_to_string(&base_file)?.trim().to_owned();
                    let i = std::fs::read_to_string(&integ_file)?.trim().to_owned();
                    (b, i)
                } else {
                    std::fs::write(&base_file, &current)?;
                    std::fs::write(&integ_file, &current)?;
                    (current.clone(), current)
                };
            repo.initialize(&base_head, &integration_head).await?;
            Ok(Self {
                descriptor,
                _lock: lock,
                engine: ConnectorEngine::Git {
                    repo,
                    base_head,
                    integration_head,
                    prepared_patch: None,
                },
            })
        } else {
            let connector =
                StateConnector::locate(&spec.path, room, &descriptor.id, &descriptor.kind)?;
            let (_base_head, integration_head) = connector.initialize(resume)?;
            Ok(Self {
                descriptor,
                _lock: lock,
                engine: ConnectorEngine::State {
                    connector,
                    integration_head,
                },
            })
        }
    }

    pub fn root(&self) -> &Path {
        match &self.engine {
            ConnectorEngine::Git { repo, .. } => &repo.root,
            ConnectorEngine::State { connector, .. } => &connector.root,
        }
    }

    pub fn head(&self) -> &str {
        match &self.engine {
            ConnectorEngine::Git {
                integration_head, ..
            }
            | ConnectorEngine::State {
                integration_head, ..
            } => integration_head,
        }
    }

    pub fn coordinator_cwd(&self) -> &Path {
        match &self.engine {
            ConnectorEngine::Git { repo, .. } => &repo.coordinator,
            ConnectorEngine::State { connector, .. } => &connector.coordinator,
        }
    }

    pub async fn sync_integration_head(&mut self, expected: Option<&str>) -> Result<()> {
        match &mut self.engine {
            ConnectorEngine::Git {
                repo,
                integration_head,
                ..
            } => {
                if repo.integration.exists() {
                    let actual = head(&repo.integration).await?;
                    if let Some(exp) = expected
                        && actual != exp
                    {
                        bail!(
                            "integration head changed; expected {exp}, found {actual}; retained work was not modified"
                        );
                    } else {
                        *integration_head = actual;
                    }
                }
            }
            ConnectorEngine::State {
                connector,
                integration_head,
                ..
            } => {
                *integration_head = connector.head()?;
            }
        }
        Ok(())
    }

    pub async fn workspace(
        &mut self,
        a: &Assignment,
        expected_head: Option<&str>,
    ) -> Result<(PathBuf, String)> {
        self.sync_integration_head(expected_head).await?;
        match &self.engine {
            ConnectorEngine::Git {
                repo,
                integration_head,
                ..
            } => {
                let path = repo.workspace(a, integration_head).await?;
                Ok((path, integration_head.clone()))
            }
            ConnectorEngine::State { connector, .. } => connector.workspace(a),
        }
    }

    pub async fn verify_assignment(
        &mut self,
        a: &Assignment,
        scopes: &[String],
        check_timeout_secs: u64,
    ) -> Result<String> {
        match &mut self.engine {
            ConnectorEngine::Git {
                repo,
                integration_head,
                prepared_patch,
                ..
            } => {
                *prepared_patch = None;
                let actual_integ = head(&repo.integration).await?;
                *integration_head = actual_integ;
                let mut scoped = a.clone();
                scoped.paths = scopes.to_vec();
                let cfg = Config {
                    room: String::new(),
                    coordinator: super::state::COORDINATOR.into(),
                    repo: repo.repo.clone(),
                    model: None,
                    goal: String::new(),
                    authorized_paths: self.descriptor.scopes.clone(),
                    configured_specialists: BTreeMap::new(),
                    checks: self.descriptor.checks.clone(),
                    max_active: 1,
                    max_calls: 1,
                    max_output_bytes: 1,
                    max_elapsed_secs: 1,
                    max_depth: 1,
                    check_timeout_secs,
                };
                let new_head = repo.verify_assignment(&scoped, &cfg).await?;
                *integration_head = new_head.clone();
                std::fs::write(repo.root.join("integration_head"), &new_head)?;
                Ok(new_head)
            }
            ConnectorEngine::State {
                connector,
                integration_head,
                ..
            } => {
                let cwd = a
                    .worktree
                    .as_deref()
                    .context("missing assignment worktree")?;
                let evidence = a.pending_result.as_ref().context("missing result")?;
                let checks: Vec<_> = self
                    .descriptor
                    .checks
                    .iter()
                    .filter(|check| evidence.checks.contains(check))
                    .cloned()
                    .collect();
                ensure!(
                    a.base.as_deref() == Some(connector.head()?.as_str()),
                    "state resource changed since assignment started; rebase in a new revision"
                );
                let new_head = connector
                    .verify_assignment(cwd, scopes, &checks, check_timeout_secs)
                    .await?;
                *integration_head = new_head.clone();
                Ok(new_head)
            }
        }
    }

    pub async fn prepare_publication(&mut self, check_timeout_secs: u64) -> Result<()> {
        match &mut self.engine {
            ConnectorEngine::Git {
                repo,
                base_head,
                integration_head,
                prepared_patch,
            } => {
                *prepared_patch = None;
                let actual_integ = head(&repo.integration).await?;
                *integration_head = actual_integ;
                let pub_file = repo.root.join("published_head");
                if pub_file.exists()
                    && std::fs::read_to_string(&pub_file)?.trim() == integration_head.as_str()
                {
                    *prepared_patch = Some(Vec::new());
                    return Ok(());
                }
                let cfg = Config {
                    room: String::new(),
                    coordinator: super::state::COORDINATOR.into(),
                    repo: repo.repo.clone(),
                    model: None,
                    goal: String::new(),
                    authorized_paths: self.descriptor.scopes.clone(),
                    configured_specialists: BTreeMap::new(),
                    checks: self.descriptor.checks.clone(),
                    max_active: 1,
                    max_calls: 1,
                    max_output_bytes: 1,
                    max_elapsed_secs: 1,
                    max_depth: 1,
                    check_timeout_secs,
                };
                let patch = repo.prepare_publication(base_head, &cfg).await?;
                *prepared_patch = Some(patch);
                Ok(())
            }
            ConnectorEngine::State { connector, .. } => {
                connector
                    .prepare_publication(
                        &self.descriptor.scopes,
                        &self.descriptor.checks,
                        check_timeout_secs,
                    )
                    .await
            }
        }
    }

    pub async fn publish(&mut self) -> Result<()> {
        match &mut self.engine {
            ConnectorEngine::Git {
                repo,
                base_head,
                integration_head,
                prepared_patch,
            } => {
                let pub_file = repo.root.join("published_head");
                if pub_file.exists()
                    && std::fs::read_to_string(&pub_file)?.trim() == integration_head.as_str()
                {
                    return Ok(());
                }
                ensure!(
                    head(&repo.integration).await? == *integration_head,
                    "integration changed after publication preparation"
                );
                let patch = prepared_patch.take().context("prepare publication first")?;
                repo.publish(base_head, &patch).await?;
                std::fs::write(pub_file, integration_head.as_bytes())?;
                Ok(())
            }
            ConnectorEngine::State { connector, .. } => connector.publish(),
        }
    }
}

fn collect_files(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    collect_files_rec(root, root, &mut files)?;
    Ok(files)
}

fn collect_files_rec(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            bail!("non-UTF8 filename in connector workspace");
        };
        if name_str == ".git" || name_str == ".buddies-connector" {
            continue;
        }
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "symlinks are not permitted in connector workspace: {}",
            path.display()
        );
        if meta.is_dir() {
            collect_files_rec(base, &path, out)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(base)?
                .to_str()
                .ok_or_else(|| anyhow!("non-UTF8 relative path"))?
                .replace('\\', "/");
            validate_path(&rel)?;
            out.insert(rel, std::fs::read(&path)?);
        }
    }
    Ok(())
}

fn digest_map(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hasher = Sha256::new();
    for (k, v) in files {
        hasher.update(k.as_bytes());
        hasher.update(b"\0");
        hasher.update((v.len() as u64).to_le_bytes());
        hasher.update(v);
    }
    data_encoding::HEXLOWER.encode(&hasher.finalize())
}

fn changed_paths(
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
) -> BTreeSet<String> {
    let mut changed = BTreeSet::new();
    for (k, v) in before {
        if after.get(k) != Some(v) {
            changed.insert(k.clone());
        }
    }
    for (k, v) in after {
        if before.get(k) != Some(v) {
            changed.insert(k.clone());
        }
    }
    changed
}

fn apply_file_changes(
    root: &Path,
    changed: &BTreeSet<String>,
    desired: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    // Stage bytes on the same filesystem before modifying existing files.
    let stage = tempfile::tempdir_in(root)?;
    let mut staged = BTreeMap::new();
    for (index, name) in changed.iter().enumerate() {
        if let Some(bytes) = desired.get(name) {
            let path = stage.path().join(index.to_string());
            std::fs::write(&path, bytes)?;
            if let Ok(meta) = std::fs::metadata(root.join(name))
                && meta.is_file()
            {
                std::fs::set_permissions(&path, meta.permissions())?;
            }
            staged.insert(name.clone(), path);
        }
    }
    for name in changed.iter().rev() {
        let path = root.join(name);
        if path.is_file() && !desired.contains_key(name) {
            std::fs::remove_file(&path)?;
        }
        let mut parent = path.parent();
        while let Some(dir) = parent.filter(|dir| *dir != root) {
            if std::fs::remove_dir(dir).is_err() {
                break;
            }
            parent = dir.parent();
        }
    }
    for (name, staged_path) in staged {
        let path = root.join(name);
        if path.is_dir() {
            std::fs::remove_dir(&path)?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(staged_path, path)?;
    }
    Ok(())
}

fn sync_tree(src: &Path, dst: &Path) -> Result<()> {
    let files = collect_files(src)?;
    if dst.exists() {
        let existing = collect_files(dst)?;
        for k in existing.keys() {
            if !files.contains_key(k) {
                let p = dst.join(k);
                if p.exists() {
                    std::fs::remove_file(p)?;
                }
            }
        }
    } else {
        std::fs::create_dir_all(dst)?;
    }
    for (k, bytes) in files {
        let p = dst.join(&k);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, bytes)?;
    }
    Ok(())
}

async fn run_checks(
    log_root: &Path,
    cwd: &Path,
    checks: &[String],
    timeout: u64,
    resource_id: &str,
    resource_kind: &str,
) -> Result<()> {
    for (index, check) in checks.iter().enumerate() {
        let log = log_root.join(format!("check-{}.log", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&log)?;
        let mut command = Command::new("sh");
        command
            .args(["-c", check])
            .current_dir(cwd)
            .env("BUDDIES_RESOURCE_ID", resource_id)
            .env("BUDDIES_RESOURCE_KIND", resource_kind)
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file)
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().context("start acceptance check")?;
        let group = crate::worker::ProcessGroup(child.id());
        let result = tokio::time::timeout(Duration::from_secs(timeout), async {
            loop {
                ensure!(
                    std::fs::metadata(&log)?.len() <= MAX_CHECK_LOG,
                    "acceptance check output exceeds 1 MiB"
                );
                if let Some(status) = child.try_wait()? {
                    return Ok::<_, anyhow::Error>(status);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        drop(group);
        let _ = child.kill().await;
        let _ = child.wait().await;
        let status = result.context("acceptance check timed out")??;
        ensure!(
            std::fs::metadata(&log)?.len() <= MAX_CHECK_LOG,
            "acceptance check output exceeds 1 MiB"
        );
        let mut output = Vec::new();
        tokio::fs::File::open(&log)
            .await?
            .take(MAX_CHECK_LOG)
            .read_to_end(&mut output)
            .await?;
        ensure!(
            status.success(),
            "acceptance check {} failed ({check}); log {}: {}",
            index + 1,
            log.display(),
            String::from_utf8_lossy(&output[..output.len().min(4096)])
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::repo::git;
    use crate::orchestrator::state::{Evidence, Status};

    fn assignment(id: &str) -> Assignment {
        serde_json::from_value(serde_json::json!({
            "id": id, "owner": "worker", "revision": 1, "description": "test",
            "paths": ["*"], "dependencies": [], "deadline": 100,
            "timeout_secs": 60, "last_heartbeat": 1, "depth": 0,
            "state": "running", "uncertain": false, "evidence": [],
            "pending_result": {"revision": 1, "summary": "done", "checks": [],
                               "verified": false, "detail": ""}
        }))
        .unwrap()
    }

    async fn git_fixture(path: &Path) {
        git(path, &["init", "-q"]).await.unwrap();
        std::fs::write(path.join("file"), "base").unwrap();
        git(path, &["add", "."]).await.unwrap();
        git(
            path,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "base",
            ],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn regression_duplicate_open_preserves_integration() {
        let tmp = tempfile::tempdir().unwrap();
        for kind in ["git", "fs"] {
            let path = tmp.path().join(kind);
            std::fs::create_dir(&path).unwrap();
            if kind == "git" {
                git_fixture(&path).await;
            }
            let spec = ResourceSpec::parse(&format!("{kind}:test={}", path.display())).unwrap();
            let resource = ManagedResource::open(spec.clone(), "room", false)
                .await
                .unwrap();
            let marker = resource.root().join("integration/retained");
            std::fs::write(&marker, "unfinished work").unwrap();
            assert!(
                ManagedResource::open(spec.clone(), "room", false)
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), "unfinished work");
            assert!(
                ManagedResource::open(spec.clone(), "room", true)
                    .await
                    .is_err()
            );
            drop(resource);
            assert!(ManagedResource::open(spec, "room", false).await.is_err());
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), "unfinished work");
        }
    }

    #[tokio::test]
    async fn regression_wildcard_allows_root_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        git_fixture(tmp.path()).await;
        let spec = ResourceSpec::parse(&format!("git:test={}", tmp.path().display())).unwrap();
        let mut resource = ManagedResource::open(spec, "room", false).await.unwrap();
        let mut a = assignment("delete");
        let (cwd, base) = resource.workspace(&a, None).await.unwrap();
        std::fs::remove_file(cwd.join("file")).unwrap();
        a.worktree = Some(cwd);
        a.base = Some(base);
        resource
            .verify_assignment(&a, &["*".into()], 5)
            .await
            .unwrap();
        resource.prepare_publication(5).await.unwrap();
        resource.publish().await.unwrap();
        assert!(!tmp.path().join("file").exists());
    }

    #[tokio::test]
    async fn regression_report_commands_never_execute_on_host() {
        let tmp = tempfile::tempdir().unwrap();
        for kind in ["git", "fs"] {
            let path = tmp.path().join(kind);
            std::fs::create_dir(&path).unwrap();
            if kind == "git" {
                git_fixture(&path).await;
            }
            let spec = ResourceSpec::parse(&format!("{kind}:test={}", path.display())).unwrap();
            let mut resource = ManagedResource::open(spec, "room", false).await.unwrap();
            let mut a = assignment("check");
            let (cwd, base) = resource.workspace(&a, None).await.unwrap();
            a.worktree = Some(cwd);
            a.base = Some(base);
            let outside = tmp.path().join(format!("outside-{kind}"));
            a.pending_result.as_mut().unwrap().checks =
                vec![format!("touch '{}'", outside.display())];
            let _ = resource.verify_assignment(&a, &["*".into()], 5).await;
            assert!(!outside.exists(), "model-provided command executed on host");
        }
    }

    #[tokio::test]
    async fn regression_late_conflict_does_not_partially_publish() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a"), "base").unwrap();
        std::fs::write(tmp.path().join("z"), "base").unwrap();
        let c = StateConnector::locate(tmp.path(), "room", "fs:test", "fs").unwrap();
        c.initialize(false).unwrap();
        std::fs::write(c.integration.join("a"), "new").unwrap();
        std::fs::write(c.integration.join("z"), "new").unwrap();
        c.prepare_publication(&["*".into()], &[], 5).await.unwrap();
        std::fs::write(tmp.path().join("z"), "human").unwrap();
        assert!(c.publish().is_err());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a")).unwrap(),
            "base"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("z")).unwrap(),
            "human"
        );
    }

    #[tokio::test]
    async fn configured_checks_gate_publication_and_selected_assignment_checks() {
        let tmp = tempfile::tempdir().unwrap();
        for kind in ["git", "fs"] {
            let path = tmp.path().join(kind);
            std::fs::create_dir(&path).unwrap();
            if kind == "git" {
                git_fixture(&path).await;
            }
            let mut spec = ResourceSpec::parse(&format!("{kind}:test={}", path.display())).unwrap();
            spec.descriptor.checks = vec!["false".into()];
            let mut resource = ManagedResource::open(spec, "room", false).await.unwrap();
            let before = resource.head().to_owned();
            let mut a = assignment("checks");
            let (cwd, base) = resource.workspace(&a, None).await.unwrap();
            std::fs::write(cwd.join("file"), "changed").unwrap();
            a.worktree = Some(cwd);
            a.base = Some(base);
            a.pending_result.as_mut().unwrap().checks = vec!["false".into()];
            assert!(
                resource
                    .verify_assignment(&a, &["*".into()], 5)
                    .await
                    .is_err()
            );
            assert_eq!(resource.head(), before);
            // A whole-goal check must not block a partial assignment before
            // dependent work exists. It still gates final publication.
            a.pending_result.as_mut().unwrap().checks = vec!["true".into()];
            resource
                .verify_assignment(&a, &["*".into()], 5)
                .await
                .unwrap();
            assert!(resource.prepare_publication(5).await.is_err());
            assert_ne!(
                std::fs::read_to_string(path.join("file")).ok().as_deref(),
                Some("changed")
            );
        }
    }

    #[tokio::test]
    async fn state_publication_rolls_back_write_errors_and_requires_preparation() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a"), "base").unwrap();
        let c = StateConnector::locate(tmp.path(), "room", "fs:test", "fs").unwrap();
        c.initialize(false).unwrap();
        std::fs::write(c.integration.join("a"), "new").unwrap();
        std::fs::write(c.integration.join("z"), "new").unwrap();
        assert!(c.publish().is_err());
        c.prepare_publication(&["*".into()], &[], 5).await.unwrap();
        // A directory containing excluded metadata cannot be replaced by a file.
        // This fails during writing, after 'a', and must roll 'a' back.
        std::fs::create_dir_all(tmp.path().join("z/.git")).unwrap();
        assert!(c.publish().unwrap_err().to_string().contains("rolled back"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a")).unwrap(),
            "base"
        );
        assert!(tmp.path().join("z/.git").exists());
        assert!(!c.root.join("publication.json").exists());
        std::fs::remove_dir_all(tmp.path().join("z")).unwrap();
        c.publish().unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("z")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn stale_prepared_snapshot_cannot_publish_new_changes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a"), "base").unwrap();
        let c = StateConnector::locate(tmp.path(), "room", "fs:test", "fs").unwrap();
        c.initialize(false).unwrap();
        std::fs::write(c.integration.join("a"), "prepared").unwrap();
        c.prepare_publication(&["*".into()], &[], 5).await.unwrap();
        std::fs::write(c.integration.join("a"), "unverified").unwrap();
        assert!(c.publish().is_err());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a")).unwrap(),
            "base"
        );
        c.prepare_publication(&["*".into()], &[], 5).await.unwrap();
        assert!(
            c.prepare_publication(&["*".into()], &["false".into()], 5)
                .await
                .is_err()
        );
        assert!(
            c.publish().is_err(),
            "a failed prepare must invalidate the previous token"
        );
    }

    #[tokio::test]
    async fn state_connector_isolates_validates_and_publishes_non_git_resources() {
        let tmp = tempfile::tempdir().unwrap();
        let linear_dir = tmp.path().join("linear-roadmap");
        std::fs::create_dir_all(linear_dir.join("issues")).unwrap();
        std::fs::write(
            linear_dir.join("issues/ENG-1.json"),
            r#"{"id":"ENG-1","state":"todo"}"#,
        )
        .unwrap();

        let mut spec =
            ResourceSpec::parse(&format!("linear:roadmap={}", linear_dir.display())).unwrap();
        assert_eq!(spec.descriptor.id, "linear:roadmap");
        assert_eq!(spec.descriptor.kind, "linear");
        spec.descriptor.scopes = vec!["issues".into()];
        spec.descriptor.checks = vec![
            r#"grep -q '"state":"done"' issues/ENG-1.json && test "$BUDDIES_RESOURCE_KIND" = "linear""#
                .into(),
        ];

        let mut resource = ManagedResource::open(spec, "test-room", false)
            .await
            .unwrap();
        let mut assignment = Assignment {
            id: "sync-ticket".into(),
            owner: "worker-linear".into(),
            revision: 1,
            description: "Close ENG-1".into(),
            paths: vec!["issues/ENG-1.json".into()],
            resources: vec![ResourceClaim {
                resource: "linear:roadmap".into(),
                scopes: vec!["issues/ENG-1.json".into()],
            }],
            dependencies: vec![],
            deadline: 100,
            timeout_secs: 60,
            last_heartbeat: 1,
            message_id: None,
            depth: 0,
            state: Status::Running,
            accepted_revision: Some(1),
            uncertain: false,
            checkpoint: None,
            worktree: None,
            base: None,
            evidence: vec![],
            pending_result: Some(Evidence {
                revision: 1,
                summary: "Closed ENG-1".into(),
                checks: vec![r#"grep -q '"state":"done"' issues/ENG-1.json"#.into()],
                verified: false,
                detail: String::new(),
            }),
        };

        let (cwd, base_head) = resource.workspace(&assignment, None).await.unwrap();
        assignment.worktree = Some(cwd.clone());
        assignment.base = Some(base_head);

        // Unauthorized scope write inside workspace must fail verification.
        std::fs::create_dir_all(cwd.join("other")).unwrap();
        std::fs::write(cwd.join("other/secret.json"), "{}").unwrap();
        assert!(
            resource
                .verify_assignment(&assignment, &["issues/ENG-1.json".into()], 5)
                .await
                .is_err()
        );
        std::fs::remove_dir_all(cwd.join("other")).unwrap();

        // Authorized write passes verification, stages in integration, and publishes only on publish().
        std::fs::write(
            cwd.join("issues/ENG-1.json"),
            r#"{"id":"ENG-1","state":"done"}"#,
        )
        .unwrap();
        resource
            .verify_assignment(&assignment, &["issues/ENG-1.json".into()], 5)
            .await
            .unwrap();
        assert!(
            std::fs::read_to_string(linear_dir.join("issues/ENG-1.json"))
                .unwrap()
                .contains("todo")
        );

        resource.prepare_publication(5).await.unwrap();
        resource.publish().await.unwrap();
        assert!(
            std::fs::read_to_string(linear_dir.join("issues/ENG-1.json"))
                .unwrap()
                .contains("done")
        );
    }
}
