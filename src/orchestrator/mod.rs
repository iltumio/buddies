//! An opt-in room orchestrator. The ordinary transport never launches a model.
mod config;
mod repo;
mod runtime;
mod state;

use anyhow::{Result, ensure};
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug, Clone)]
pub struct Options {
    /// Path to a machine orchestrator configuration file (TOML or JSON).
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub room: Option<String>,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Goal to orchestrate immediately. Omit to run in service mode accepting delegated tasks.
    #[arg(long)]
    pub goal: Option<String>,
    /// Repository-relative files/directories the goal may change. Repeat as needed.
    #[arg(long = "allow-path")]
    pub authorized_paths: Vec<String>,
    /// Shared acceptance command, run in the integration worktree. Repeat as needed.
    #[arg(long = "check")]
    pub checks: Vec<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, env = "BUDDIES_URL")]
    pub url: Option<String>,
    #[arg(long)]
    pub codex_bin: Option<PathBuf>,
    /// Optional directory for orchestrator state database (defaults to primary repo .git dir).
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    #[arg(long)]
    pub max_active_specialists: Option<usize>,
    #[arg(long)]
    pub max_model_calls: Option<u64>,
    #[arg(long)]
    pub max_output_bytes: Option<u64>,
    #[arg(long)]
    pub max_elapsed_secs: Option<u64>,
    #[arg(long)]
    pub max_depth: Option<usize>,
    #[arg(long)]
    pub check_timeout_secs: Option<u64>,
    /// Debounce addressed messages and decision events before starting a turn.
    #[arg(long)]
    pub debounce_ms: Option<u64>,
    /// Resume this room's saved graph and conversations; never replay uncertain work.
    #[arg(long)]
    pub resume: bool,
    /// In service mode (when --goal is omitted), handle a single delegated task and exit.
    #[arg(long)]
    pub once: bool,
}

pub async fn run(options: Options) -> Result<()> {
    let resolved = config::Resolved::from_options(options)?;
    let repos = repo::Repositories::locate_all(&resolved.config).await?;
    let primary_root = repos.primary().root.clone();
    let state_path = resolved
        .state_dir
        .as_ref()
        .map(|d| d.join("state.redb"))
        .unwrap_or_else(|| primary_root.join("state.redb"));
    let mut mirrors: Vec<PathBuf> = repos
        .repos
        .values()
        .map(|r| r.root.join("state.json"))
        .filter(|p| *p != state_path.with_extension("json"))
        .collect();
    mirrors.dedup();
    let store = state::Store::open_with_mirrors(state_path, mirrors)?;

    if resolved.service_mode {
        ensure!(
            !resolved.resume,
            "--resume requires an explicit --goal; service mode accepts new delegated tasks"
        );
        return runtime::run_service(resolved, store, repos).await;
    }

    let mut state = if let Some(saved) = store.load()? {
        ensure!(
            resolved.resume,
            "saved room exists; use --resume with the same goal, scope and budgets"
        );
        ensure!(
            saved.version == 1 && saved.config == resolved.config,
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
        ensure!(!resolved.resume, "no saved orchestrator in this room");
        let bases = repos.snapshot_bases(true).await?;
        state::State::with_resources(
            resolved.config.clone(),
            bases,
            resolved.initial_specialists.clone(),
        )
    };
    state.recover_interrupted();
    state.stopped_reason = None;
    store.save(&state)?;
    repos.initialize_all(&state).await?;
    let start_msg = if resolved.from_config_file {
        "Start/resume coordination. Inspect the saved graph, configured machine resources, and uncertain effects before acting. Create specialists and grant them resource access via create_specialist, then assign bounded work."
    } else {
        "Start/resume coordination. Inspect the saved graph and uncertain effects before acting. Use the two existing specialist roles first."
    };
    state.decision(start_msg.into());
    store.save(&state)?;
    runtime::run_single(resolved, state, store, repos).await
}
