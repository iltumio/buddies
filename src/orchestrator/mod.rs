//! An opt-in room orchestrator. The ordinary transport never launches a model.
mod repo;
mod runtime;
mod state;

use anyhow::{Context, Result, ensure};
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct Options {
    #[arg(long)]
    pub room: String,
    #[arg(long)]
    pub repo: PathBuf,
    #[arg(long)]
    pub goal: String,
    /// Repository-relative files/directories the goal may change. Repeat as needed.
    #[arg(long = "allow-path", required = true)]
    pub authorized_paths: Vec<String>,
    /// Shared acceptance command, run in the integration worktree. Repeat as needed.
    #[arg(long = "check", required = true)]
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

pub async fn run(mut options: Options) -> Result<()> {
    options.repo = options
        .repo
        .canonicalize()
        .context("repository does not exist")?;
    ensure!(
        !options.room.trim().is_empty() && options.room.len() <= 256,
        "invalid room"
    );
    state::text(&options.goal)?;
    ensure!(
        (1..=16).contains(&options.max_active_specialists),
        "max active specialists must be 1..16"
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
        options.authorized_paths.len() <= 32 && options.checks.len() <= 8,
        "too many paths/checks"
    );
    for path in &options.authorized_paths {
        state::validate_path(path)?;
    }
    for check in &options.checks {
        state::text(check)?;
    }
    let config = state::Config {
        room: options.room.clone(),
        repo: options.repo.clone(),
        model: options.model.clone(),
        goal: options.goal.clone(),
        authorized_paths: options.authorized_paths.clone(),
        checks: options.checks.clone(),
        max_active: options.max_active_specialists,
        max_calls: options.max_model_calls,
        max_output_bytes: options.max_output_bytes,
        max_elapsed_secs: options.max_elapsed_secs,
        max_depth: options.max_depth,
        check_timeout_secs: options.check_timeout_secs,
    };
    let repo = repo::Repository::locate(&options.repo, &options.room).await?;
    let store = state::Store::open(repo.root.join("state.redb"))?;
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
        let base = repo::head(&options.repo).await?;
        repo.check_checkout(&base).await?;
        state::State::new(config, base)
    };
    state.recover_interrupted();
    state.stopped_reason = None;
    store.save(&state)?;
    repo.initialize(&state.base, &state.integration_head)
        .await?;
    state.decision("Start/resume coordination. Inspect the saved graph and uncertain effects before acting. Use the two existing specialist roles first.".into());
    store.save(&state)?;
    runtime::run(options, state, store, repo).await
}
