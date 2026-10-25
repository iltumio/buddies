//! Isolated writes and verification. The user's checkout is touched only by a
//! final, verified patch, with both HEAD and cleanliness checked again.
use super::state::{Assignment, Config, ResourceConfig, State, Status, contains, validate_path};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

const MAX_PATCH: usize = 4 * 1024 * 1024;
const MAX_CHECK_LOG: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct Repository {
    pub root: PathBuf,
    pub integration: PathBuf,
    pub coordinator: PathBuf,
    pub repo: PathBuf,
}

#[derive(Clone)]
pub struct Repositories {
    pub primary: String,
    pub repos: BTreeMap<String, Repository>,
}

pub async fn git(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}
pub async fn head(cwd: &Path) -> Result<String> {
    Ok(String::from_utf8(git(cwd, &["rev-parse", "HEAD"]).await?)?
        .trim()
        .into())
}
async fn clean(cwd: &Path) -> Result<()> {
    ensure!(
        git(cwd, &["status", "--porcelain", "--untracked-files=all"])
            .await?
            .is_empty(),
        "repository must be clean; existing effects were retained at {}",
        cwd.display()
    );
    Ok(())
}

impl Repository {
    pub async fn locate(repo: &Path, room: &str) -> Result<Self> {
        let top = String::from_utf8(git(repo, &["rev-parse", "--show-toplevel"]).await?)?;
        ensure!(
            Path::new(top.trim()).canonicalize()? == repo,
            "--repo must be the repository root"
        );
        let common = String::from_utf8(
            git(
                repo,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await?,
        )?;
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(room.as_bytes()));
        let root = Path::new(common.trim())
            .join("buddies-orchestrator")
            .join(&hash[..24]);
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            integration: root.join("integration"),
            coordinator: root.join("coordinator"),
            root,
            repo: repo.into(),
        })
    }
    pub async fn initialize(&self, base: &str, integration_head: &str) -> Result<()> {
        self.worktree(&self.coordinator, base).await?;
        self.worktree(&self.integration, integration_head).await?;
        ensure!(
            head(&self.integration).await? == integration_head,
            "integration effects are uncertain; inspect the retained worktree before resuming"
        );
        clean(&self.integration).await?;
        Ok(())
    }
    pub async fn reset(&self, base: &str) -> Result<()> {
        self.worktree(&self.coordinator, base).await?;
        git(&self.coordinator, &["checkout", "--detach", "-f", base]).await?;
        git(&self.coordinator, &["clean", "-fd"]).await?;
        self.worktree(&self.integration, base).await?;
        git(&self.integration, &["checkout", "--detach", "-f", base]).await?;
        git(&self.integration, &["clean", "-fd"]).await?;
        Ok(())
    }
    pub async fn prepare_service(&self, base: &str) -> Result<()> {
        if self.check_checkout(base).await.is_ok() {
            self.reset(base).await?;
        } else {
            // The coordinator must still start for a dirty resource. Create
            // missing worktrees without resetting any retained effects.
            self.worktree(&self.coordinator, base).await?;
            self.worktree(&self.integration, base).await?;
        }
        Ok(())
    }
    async fn worktree(&self, path: &Path, base: &str) -> Result<()> {
        if path.exists() {
            ensure!(
                path.join(".git").is_file(),
                "existing worktree path is invalid"
            );
        } else {
            git(
                &self.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    path.to_str().context("non-UTF8 worktree")?,
                    base,
                ],
            )
            .await?;
        }
        Ok(())
    }
    pub async fn workspace(&self, a: &Assignment, integration_head: &str) -> Result<PathBuf> {
        // Every revision/explicit retry gets a fresh worktree. Old effects survive.
        let path = self
            .root
            .join(format!("{}-r{}-{}", a.id, a.revision, uuid::Uuid::new_v4()));
        self.worktree(&path, integration_head).await?;
        Ok(path)
    }
    async fn validate_changes(cwd: &Path, base: &str, paths: &[String]) -> Result<()> {
        // Include staged, unstaged, committed, and untracked files. Disable rename
        // detection so both sides of a move must be authorized.
        let mut names = git(cwd, &["diff", "--name-only", "--no-renames", "-z", base]).await?;
        names.extend(git(cwd, &["ls-files", "--others", "--exclude-standard", "-z"]).await?);
        let allow_all = paths.is_empty() || paths.iter().any(|p| p == "." || p == "*");
        for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
            let name = std::str::from_utf8(name).context("non-UTF8 changed path")?;
            validate_path(name)?;
            ensure!(
                allow_all || paths.iter().any(|p| contains(p, name)),
                "write outside assignment ownership: {name}"
            );
            // No new symbolic links/submodules that could redirect checks outside
            // the assignment workspace. Deleting a pre-existing link is allowed.
            if let Ok(meta) = std::fs::symlink_metadata(cwd.join(name)) {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "changed symlink is not an owned regular file: {name}"
                );
            }
            ensure!(
                !cwd.join(name).join(".git").exists(),
                "changed nested repository/submodule: {name}"
            );
        }
        Ok(())
    }
    async fn patch(cwd: &Path, base: &str, paths: &[String]) -> Result<Vec<u8>> {
        Self::validate_changes(cwd, base, paths).await?;
        git(cwd, &["add", "--all", "--", "."]).await?;
        let patch = git(
            cwd,
            &[
                "diff",
                "--cached",
                "--binary",
                "--full-index",
                "--no-renames",
                base,
            ],
        )
        .await?;
        ensure!(patch.len() <= MAX_PATCH, "patch exceeds 4 MiB");
        Ok(patch)
    }
    async fn apply(cwd: &Path, patch: &[u8], check: bool, index: bool) -> Result<()> {
        if patch.is_empty() {
            return Ok(());
        }
        let mut command = Command::new("git");
        command.arg("apply");
        if check {
            command.arg("--check");
        }
        if index {
            command.arg("--index");
        }
        let mut child = command
            .arg("--binary")
            .arg("-")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        child.stdin.take().unwrap().write_all(patch).await?;
        let output = child.wait_with_output().await?;
        ensure!(
            output.status.success(),
            "integration conflict: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
    pub async fn verify_assignment(&self, a: &Assignment, config: &Config) -> Result<String> {
        let cwd = a
            .worktree
            .as_deref()
            .context("missing assignment worktree")?;
        let base = a.base.as_deref().context("missing assignment base")?;
        Self::validate_changes(cwd, base, &a.paths).await?;
        let evidence = a.pending_result.as_ref().context("missing result")?;
        self.checks(cwd, &evidence.checks, config.check_timeout_secs)
            .await?;
        let patch = Self::patch(cwd, base, &a.paths).await?;
        std::fs::write(
            self.root.join(format!("{}-r{}.patch", a.id, a.revision)),
            &patch,
        )?;
        clean(&self.integration).await?;
        Self::apply(&self.integration, &patch, true, true).await?;
        Self::apply(&self.integration, &patch, false, true).await?;
        if !patch.is_empty() {
            git(
                &self.integration,
                &[
                    "-c",
                    "user.name=buddies",
                    "-c",
                    "user.email=buddies@localhost",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--no-verify",
                    "-m",
                    &format!("buddies: {} revision {}", a.id, a.revision),
                ],
            )
            .await?;
        }
        head(&self.integration).await
    }
    pub async fn prepare_publication_resource(
        &self,
        base: &str,
        resource: &ResourceConfig,
        check_timeout_secs: u64,
    ) -> Result<Vec<u8>> {
        self.checks(&self.integration, &resource.checks, check_timeout_secs)
            .await?;
        clean(&self.integration).await?;
        let patch = Self::patch(&self.integration, base, &resource.authorized_paths).await?;
        let path = self.root.join("verified.patch");
        std::fs::write(&path, &patch)?;
        ensure!(
            head(&self.repo).await? == base,
            "user checkout HEAD changed; verified patch retained at {}",
            path.display()
        );
        clean(&self.repo).await?;
        Self::apply(&self.repo, &patch, true, false).await?;
        Ok(patch)
    }
    pub async fn publish(&self, base: &str, patch: &[u8]) -> Result<()> {
        self.check_checkout(base).await?;
        Self::apply(&self.repo, patch, false, false).await?;
        Ok(())
    }
    pub async fn check_checkout(&self, base: &str) -> Result<()> {
        clean(&self.repo).await?;
        ensure!(
            head(&self.repo).await? == base,
            "user checkout HEAD changed"
        );
        Ok(())
    }
    pub async fn checks(&self, cwd: &Path, checks: &[String], timeout: u64) -> Result<()> {
        for (index, check) in checks.iter().enumerate() {
            let log = self
                .root
                .join(format!("check-{}.log", uuid::Uuid::new_v4()));
            let file = std::fs::File::create(&log)?;
            let mut command = Command::new("sh");
            command
                .args(["-c", check])
                .current_dir(cwd)
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
}

impl Repositories {
    pub async fn locate_all(config: &Config) -> Result<Self> {
        let mut repos = BTreeMap::new();
        for (name, res) in config.all_resources() {
            let repo = Repository::locate(&res.repo, &config.room).await?;
            repos.insert(name, repo);
        }
        Ok(Self {
            primary: config.primary_resource_name(),
            repos,
        })
    }

    pub fn primary(&self) -> &Repository {
        self.repos
            .get(&self.primary)
            .or_else(|| self.repos.values().next())
            .expect("at least one repository")
    }

    pub fn get(&self, resource: &str) -> Result<&Repository> {
        if let Some(r) = self.repos.get(resource) {
            return Ok(r);
        }
        if resource == "default" && self.repos.len() == 1 {
            return Ok(self.primary());
        }
        anyhow::bail!("unknown repository resource '{resource}'")
    }

    pub async fn snapshot_bases(
        &self,
        require_all_clean: bool,
    ) -> Result<BTreeMap<String, String>> {
        let mut bases = BTreeMap::new();
        for (name, repo) in &self.repos {
            let b = head(&repo.repo).await?;
            if require_all_clean {
                repo.check_checkout(&b).await?;
            }
            bases.insert(name.clone(), b);
        }
        Ok(bases)
    }

    pub async fn initialize_all(&self, state: &State) -> Result<()> {
        for (name, repo) in &self.repos {
            let base = state.resource_base(name);
            let head = state.resource_head(name);
            repo.initialize(&base, &head).await?;
        }
        Ok(())
    }

    pub async fn prepare_all_publications(
        &self,
        state: &State,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut patches = BTreeMap::new();
        let all_res = state.config.all_resources();
        for (name, res) in &all_res {
            let touched = all_res.len() == 1
                || state
                    .assignments
                    .values()
                    .any(|a| a.state == Status::Completed && a.resource == *name);
            if !touched {
                continue;
            }
            let repo = self.get(name)?;
            let base = state.resource_base(name);
            let patch = repo
                .prepare_publication_resource(&base, res, state.config.check_timeout_secs)
                .await?;
            patches.insert(name.clone(), patch);
        }
        Ok(patches)
    }

    pub async fn publish_all(
        &self,
        state: &State,
        patches: &BTreeMap<String, Vec<u8>>,
    ) -> Result<()> {
        for (name, patch) in patches {
            let repo = self.get(name)?;
            let base = state.resource_base(name);
            repo.publish(&base, patch).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn service_starts_with_dirty_primary_checkout() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let cwd = tmp.path();
        git(cwd, &["init"]).await?;
        git(
            cwd,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        )
        .await?;
        let base = head(cwd).await?;
        std::fs::write(cwd.join("local.txt"), "retain user changes")?;
        let repo = Repository::locate(&cwd.canonicalize()?, "service-test").await?;
        repo.prepare_service(&base).await?;
        // Exercise the same process-spawn boundary as the Codex app-server,
        // without starting a model or requiring Codex to be installed.
        let status = Command::new("git")
            .arg("--version")
            .current_dir(&repo.coordinator)
            .stdout(Stdio::null())
            .status()
            .await
            .context("start orchestrator Codex app-server")?;
        assert!(status.success());
        assert_eq!(head(&repo.integration).await?, base);
        assert_eq!(
            std::fs::read_to_string(cwd.join("local.txt"))?,
            "retain user changes"
        );
        assert!(repo.check_checkout(&base).await.is_err());
        // A second task must not discard effects retained in the coordinator.
        std::fs::write(repo.coordinator.join("retained.txt"), "prior effects")?;
        std::fs::write(repo.integration.join("retained.txt"), "integration effects")?;
        repo.prepare_service(&base).await?;
        assert_eq!(
            std::fs::read_to_string(repo.coordinator.join("retained.txt"))?,
            "prior effects"
        );
        assert_eq!(
            std::fs::read_to_string(repo.integration.join("retained.txt"))?,
            "integration effects"
        );
        Ok(())
    }
}
