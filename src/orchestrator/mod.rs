//! An opt-in room orchestrator. The ordinary transport never launches a model.
pub mod connector;
mod machine;
pub(crate) mod repo;
mod runtime;
pub mod specialist_worker;
pub(crate) mod state;

use anyhow::{Context, Result, ensure};
use clap::Args;
use connector::{ManagedResource, ResourceDescriptor, ResourceSpec};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Args, Debug, Clone)]
#[group(id = "OrchestratorOptions")]
pub struct Options {
    #[command(flatten)]
    machine: machine::Options,
    /// Stable name for goal orchestration with local or remote specialists.
    #[arg(long)]
    name: Option<String>,
    /// Managed connector (`[<kind>:]<id>=<path>`).
    #[arg(long = "resource")]
    resources: Vec<String>,
    /// Specialist boundaries (`<name>=<resource>:<scope>`).
    #[arg(long = "specialist")]
    specialists: Vec<String>,
}

pub async fn run(options: Options) -> Result<()> {
    let machine_mode = options.machine.config.is_some() || options.machine.goal.is_none();
    if machine_mode {
        ensure!(
            options.name.is_none()
                && options.resources.is_empty()
                && options.specialists.is_empty(),
            "--config/service mode uses resources and specialists from the machine config; --name, --resource and --specialist require goal mode without --config"
        );
        return machine::run(options.machine).await;
    }
    let m = options.machine;
    ensure!(!m.once, "--once requires service mode without --goal");
    run_goal(GoalOptions {
        room: m.room.context("--room is required without --config")?,
        name: options.name.unwrap_or_else(|| state::COORDINATOR.into()),
        repo: m.repo,
        resources: options.resources,
        specialists: options.specialists,
        state_dir: m.state_dir,
        goal: m.goal.expect("goal mode"),
        authorized_paths: m.authorized_paths,
        checks: m.checks,
        model: m.model,
        url: m.url.unwrap_or_else(|| "http://127.0.0.1:8080".into()),
        codex_bin: m.codex_bin.unwrap_or_else(|| "codex".into()),
        max_active_specialists: m.max_active_specialists.unwrap_or(2),
        max_model_calls: m.max_model_calls.unwrap_or(64),
        max_output_bytes: m.max_output_bytes.unwrap_or(1048576),
        max_elapsed_secs: m.max_elapsed_secs.unwrap_or(3600),
        max_depth: m.max_depth.unwrap_or(8),
        check_timeout_secs: m.check_timeout_secs.unwrap_or(120),
        debounce_ms: m.debounce_ms.unwrap_or(1000),
        resume: m.resume,
    })
    .await
}

#[derive(Args)]
pub struct GoalOptions {
    #[arg(long)]
    pub room: String,
    /// Stable participant name for this local orchestrator in the room (allows multiple local orchestrators to coordinate).
    #[arg(long, default_value = state::COORDINATOR)]
    pub name: String,
    /// Optional local git repository for single-repo orchestration.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Managed local resources/connectors (`[<kind>:]<id>=<path>`, e.g. `git:frontend=/path/fe`, `linear:roadmap=/path/linear`).
    #[arg(long = "resource")]
    pub resources: Vec<String>,
    /// Local specialists and their strict boundaries (`<name>=<boundary1>[,<boundary2>...]`, e.g. `frontend=src/frontend` or `fe=git:frontend:src`).
    #[arg(long = "specialist")]
    pub specialists: Vec<String>,
    /// Optional state directory when running without `--repo`.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    #[arg(long)]
    pub goal: String,
    /// Repository-relative files/directories the goal may change. Optional when `--specialist` or `--resource` boundaries are provided.
    #[arg(long = "allow-path", visible_alias = "allow-scope")]
    pub authorized_paths: Vec<String>,
    /// Shared acceptance command, run in the integration worktree.
    #[arg(long = "check")]
    pub checks: Vec<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, env = "BUDDIES_URL", default_value = "http://127.0.0.1:8080")]
    pub url: String,
    #[arg(long, default_value = "codex")]
    pub codex_bin: PathBuf,
    #[arg(long, default_value_t = 2)]
    pub max_active_specialists: usize,
    #[arg(long, default_value_t = 64)]
    pub max_model_calls: u64,
    #[arg(long, default_value_t = 1048576)]
    pub max_output_bytes: u64,
    #[arg(long, default_value_t = 3600)]
    pub max_elapsed_secs: u64,
    #[arg(long, default_value_t = 8)]
    pub max_depth: usize,
    #[arg(long, default_value_t = 120)]
    pub check_timeout_secs: u64,
    /// Debounce addressed messages and decision events before starting a turn.
    #[arg(long, default_value_t = 1000)]
    pub debounce_ms: u64,
    /// Resume this room's saved graph and conversations; never replay uncertain work.
    #[arg(long)]
    pub resume: bool,
}

pub(crate) async fn run_goal(mut options: GoalOptions) -> Result<()> {
    if let Some(repo) = &options.repo {
        options.repo = Some(repo.canonicalize().context("repository does not exist")?);
    }
    ensure!(
        !options.room.trim().is_empty() && options.room.len() <= 256,
        "invalid room"
    );
    crate::conversation::validate_name(&options.name)?;
    state::text(&options.goal)?;
    ensure!(
        (1..=32).contains(&options.max_active_specialists),
        "max active specialists must be 1..32"
    );
    ensure!(
        options.max_model_calls > 0
            && options.max_model_calls <= 10000
            && options.max_output_bytes > 0
            && options.max_output_bytes <= 64 * 1024 * 1024
            && options.max_elapsed_secs > 0
            && options.max_elapsed_secs <= 7 * 86400
            && options.check_timeout_secs > 0
            && options.check_timeout_secs <= options.max_elapsed_secs
            && options.max_depth <= 32
            && (50..=10000).contains(&options.debounce_ms),
        "invalid runtime budget/debounce"
    );
    ensure!(
        options.authorized_paths.len() <= 32
            && options.checks.len() <= 8
            && options.resources.len() <= 16
            && options.specialists.len() <= 64,
        "too many paths/checks/resources/specialists"
    );
    for check in &options.checks {
        state::text(check)?;
    }

    let room_key = if options.name == state::COORDINATOR {
        options.room.clone()
    } else {
        format!("{}:{}", options.room, options.name)
    };

    let mut managed_resources: BTreeMap<String, ManagedResource> = BTreeMap::new();
    let mut parsed_specs = Vec::new();
    let mut resource_descriptors: Vec<ResourceDescriptor> = Vec::new();
    if !options.resources.is_empty() {
        for raw in &options.resources {
            let spec = ResourceSpec::parse(raw)?;
            ensure!(
                !parsed_specs
                    .iter()
                    .any(|s: &ResourceSpec| s.descriptor.id == spec.descriptor.id),
                "duplicate resource id '{}'",
                spec.descriptor.id
            );
            parsed_specs.push(spec);
        }
        let multi = parsed_specs.len() > 1;
        for raw_scope in &options.authorized_paths {
            if let Some((spec, rest)) = parsed_specs.iter_mut().find_map(|s| {
                raw_scope
                    .strip_prefix(&format!("{}:", s.descriptor.id))
                    .or_else(|| {
                        s.descriptor
                            .id
                            .split_once(':')
                            .and_then(|(_, short)| raw_scope.strip_prefix(&format!("{short}:")))
                    })
                    .map(|r| (s, r))
            }) {
                if rest != "*" {
                    state::validate_path(rest)?;
                }
                spec.descriptor.scopes.push(rest.to_owned());
            } else if !multi {
                if raw_scope != "*" {
                    state::validate_path(raw_scope)?;
                }
                parsed_specs[0].descriptor.scopes.push(raw_scope.clone());
            } else {
                for spec in &mut parsed_specs {
                    if raw_scope != "*" {
                        state::validate_path(raw_scope)?;
                    }
                    spec.descriptor.scopes.push(raw_scope.clone());
                }
            }
        }
        for raw_check in &options.checks {
            if let Some((spec, rest)) = parsed_specs.iter_mut().find_map(|s| {
                raw_check
                    .strip_prefix(&format!("{}:", s.descriptor.id))
                    .or_else(|| {
                        s.descriptor
                            .id
                            .split_once(':')
                            .and_then(|(_, short)| raw_check.strip_prefix(&format!("{short}:")))
                    })
                    .map(|r| (s, r))
            }) {
                state::text(rest)?;
                spec.descriptor.checks.push(rest.to_owned());
            } else {
                for spec in &mut parsed_specs {
                    spec.descriptor.checks.push(raw_check.clone());
                }
            }
        }
        parsed_specs.sort_by(|a, b| a.descriptor.id.cmp(&b.descriptor.id));
        for spec in &mut parsed_specs {
            if spec.descriptor.scopes.is_empty() {
                spec.descriptor.scopes.push("*".into());
            }
            resource_descriptors.push(spec.descriptor.clone());
        }
    } else {
        for path in &options.authorized_paths {
            state::validate_path(path)?;
        }
    }

    let configured_specialists = if !options.specialists.is_empty() {
        ResourceSpec::parse_specialist_boundaries(
            &options.specialists,
            &options.name,
            &resource_descriptors,
            &options.checks,
        )?
    } else if !resource_descriptors.is_empty() {
        // Default specialist per configured resource when `--specialist` is omitted.
        let mut defaults = BTreeMap::new();
        for desc in &resource_descriptors {
            let short = desc.id.split_once(':').map(|(_, s)| s).unwrap_or(&desc.id);
            let spec_name = format!("specialist-{short}");
            crate::conversation::validate_name(&spec_name)?;
            defaults.insert(spec_name, vec![desc.clone()]);
        }
        defaults
    } else {
        BTreeMap::new()
    };

    if options.resources.is_empty() && !configured_specialists.is_empty() {
        if options.authorized_paths.is_empty() {
            let mut union_paths = Vec::new();
            for descs in configured_specialists.values() {
                for d in descs {
                    if d.id == "default" {
                        for sc in &d.scopes {
                            if sc != "*" && !union_paths.contains(sc) {
                                union_paths.push(sc.clone());
                            }
                        }
                    }
                }
            }
            options.authorized_paths = union_paths;
        } else {
            for (spec_name, descs) in &configured_specialists {
                for d in descs {
                    if d.id == "default" {
                        for sc in &d.scopes {
                            ensure!(
                                options
                                    .authorized_paths
                                    .iter()
                                    .any(|p| state::contains(p, sc)),
                                "specialist '{spec_name}' boundary '{sc}' is outside --allow-path"
                            );
                        }
                    }
                }
            }
        }
    }

    let (repo_opt, root_dir, coordinator_cwd) = if let Some(repo_path) = &options.repo {
        let r = repo::Repository::locate(repo_path, &room_key).await?;
        let root = r.root.clone();
        let coord = r.coordinator.clone();
        (Some(r), root, coord)
    } else if let Some(first_spec) = parsed_specs.first()
        && options.state_dir.is_none()
    {
        let (root, coord) = ManagedResource::paths(first_spec, &room_key).await?;
        (None, root, coord)
    } else {
        let base_dir = match &options.state_dir {
            Some(d) => {
                std::fs::create_dir_all(d)?;
                d.canonicalize()?
            }
            None => std::env::current_dir()?.join(".buddies-orchestrator"),
        };
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(room_key.as_bytes()));
        let root = base_dir.join(&hash[..24]);
        let coord = root.join("coordinator");
        std::fs::create_dir_all(&coord)?;
        (None, root, coord)
    };

    let config = state::Config {
        room: options.room.clone(),
        coordinator: options.name.clone(),
        repo: options.repo.clone().unwrap_or_else(|| root_dir.clone()),
        model: options.model.clone(),
        goal: options.goal.clone(),
        authorized_paths: options.authorized_paths.clone(),
        configured_specialists,
        checks: options.checks.clone(),
        max_active: options.max_active_specialists,
        max_calls: options.max_model_calls,
        max_output_bytes: options.max_output_bytes,
        max_elapsed_secs: options.max_elapsed_secs,
        max_depth: options.max_depth,
        check_timeout_secs: options.check_timeout_secs,
    };

    let store = state::Store::open(root_dir.join("state.redb"))?;
    let mut state = if let Some(saved) = store.load()? {
        ensure!(
            options.resume,
            "saved room exists; use --resume with the same goal, scope and budgets"
        );
        ensure!(
            saved.version == 1 && saved.config == config,
            "saved orchestrator configuration differs"
        );
        ensure!(
            !saved.publishing,
            "publication outcome uncertain; inspect verified.patch and the user checkout before any retry"
        );
        if saved.completed {
            println!(
                "{}",
                saved.final_summary.as_deref().unwrap_or("Goal completed")
            );
            return Ok(());
        }
        saved
    } else {
        ensure!(!options.resume, "no saved orchestrator in this room");
        let base = if let Some(repo) = &repo_opt {
            let b = repo::head(&repo.repo).await?;
            repo.check_checkout(&b).await?;
            b
        } else {
            "0".repeat(40)
        };
        state::State::new(config, base)
    };
    // Only after acquiring the room store and validating resume/config may a
    // connector initialize or restore integration state.
    for spec in parsed_specs {
        let res = ManagedResource::open(spec, &room_key, options.resume).await?;
        managed_resources.insert(res.descriptor.id.clone(), res);
    }
    for (id, res) in &managed_resources {
        state
            .resource_heads
            .insert(id.clone(), res.head().to_owned());
    }
    state.recover_interrupted();
    state.stopped_reason = None;
    store.save(&state)?;
    if let Some(repo) = &repo_opt {
        repo.initialize(&state.base, &state.integration_head)
            .await?;
    }
    state.decision(
        "Start/resume coordination. Inspect the saved graph, specialists and their boundaries, peer orchestrators in the room, and uncertain effects before acting."
            .into(),
    );
    store.save(&state)?;
    runtime::run(
        options,
        state,
        store,
        repo_opt,
        managed_resources,
        root_dir,
        coordinator_cwd,
    )
    .await
}
