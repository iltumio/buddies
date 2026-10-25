//! Isolated writes and verification. The user's checkout is touched only by a
//! final, verified patch, with both HEAD and cleanliness checked again.
use super::state::{Assignment, Config, contains};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
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

pub struct Repository {
    pub root: PathBuf,
    pub integration: PathBuf,
    pub coordinator: PathBuf,
    pub repo: PathBuf,
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
        for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
            let name = std::str::from_utf8(name).context("non-UTF8 changed path")?;
            super::state::validate_path(name)?;
            ensure!(
                paths.iter().any(|p| contains(p, name)),
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
        // Shared checks can depend on later assignments. Only run the reported
        // subset explicitly authorized by the user here; run ALL at publication.
        let checks: Vec<_> = config
            .checks
            .iter()
            .filter(|check| evidence.checks.contains(check))
            .cloned()
            .collect();
        self.checks(cwd, &checks, config.check_timeout_secs).await?;
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
    pub async fn prepare_publication(&self, base: &str, config: &Config) -> Result<Vec<u8>> {
        self.checks(&self.integration, &config.checks, config.check_timeout_secs)
            .await?;
        clean(&self.integration).await?;
        let patch = Self::patch(&self.integration, base, &config.authorized_paths).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn ownership_includes_committed_untracked_staged_and_renamed_files() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]).await.unwrap();
        std::fs::write(tmp.path().join("owned"), "before").unwrap();
        std::fs::write(tmp.path().join("other"), "before").unwrap();
        git(tmp.path(), &["add", "."]).await.unwrap();
        git(
            tmp.path(),
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
        let base = head(tmp.path()).await.unwrap();
        let paths = vec!["owned".into()];
        std::fs::write(tmp.path().join("untracked"), "outside").unwrap();
        assert!(Repository::patch(tmp.path(), &base, &paths).await.is_err());
        std::fs::remove_file(tmp.path().join("untracked")).unwrap();
        std::fs::write(tmp.path().join("other"), "outside").unwrap();
        git(tmp.path(), &["add", "."]).await.unwrap();
        assert!(Repository::patch(tmp.path(), &base, &paths).await.is_err());
        git(
            tmp.path(),
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "outside",
            ],
        )
        .await
        .unwrap();
        assert!(Repository::patch(tmp.path(), &base, &paths).await.is_err());
        git(tmp.path(), &["reset", "--hard", &base]).await.unwrap();
        std::fs::rename(tmp.path().join("other"), tmp.path().join("owned")).unwrap();
        assert!(Repository::patch(tmp.path(), &base, &paths).await.is_err());
    }

    #[tokio::test]
    async fn verification_gates_publication_and_preserves_concurrent_user_edits() {
        use crate::orchestrator::state::{self, COORDINATOR, Control, State};
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]).await.unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/file"), "base").unwrap();
        git(tmp.path(), &["add", "."]).await.unwrap();
        git(
            tmp.path(),
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
        let base = head(tmp.path()).await.unwrap();
        let config = state::test_config(tmp.path().to_path_buf());
        let mut state = State::new(config.clone(), base.clone());
        state
            .apply(
                COORDINATOR,
                Control::Assign {
                    id: "task".into(),
                    owner: "specialist-a".into(),
                    description: "edit file".into(),
                    paths: vec!["src/file".into()],
                    resource: None,
                    resources: vec![],
                    dependencies: vec![],
                    deadline_secs: 60,
                },
                1,
            )
            .unwrap();
        let repo = Repository::locate(tmp.path(), "test").await.unwrap();
        repo.initialize(&base, &base).await.unwrap();
        let cwd = repo
            .workspace(&state.assignments["task"], &base)
            .await
            .unwrap();
        let a = state.assignments.get_mut("task").unwrap();
        a.worktree = Some(cwd.clone());
        a.base = Some(base.clone());
        a.state = state::Status::Running;
        state
            .apply(
                "specialist-a",
                Control::Accept {
                    id: "task".into(),
                    revision: 1,
                },
                2,
            )
            .unwrap();
        state
            .apply(
                "specialist-a",
                Control::Report {
                    id: "task".into(),
                    revision: 1,
                    summary: "edited".into(),
                    checks: vec!["test -f src/file".into()],
                },
                2,
            )
            .unwrap();
        std::fs::write(cwd.join("src/file"), "verified").unwrap();
        let integrated = repo
            .verify_assignment(&state.assignments["task"], &config)
            .await
            .unwrap();
        assert_ne!(integrated, base);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/file")).unwrap(),
            "base"
        );
        let mut failing = config.clone();
        failing.checks = vec!["false".into()];
        assert!(repo.prepare_publication(&base, &failing).await.is_err());
        let patch = repo.prepare_publication(&base, &config).await.unwrap();
        std::fs::write(tmp.path().join("src/file"), "human edit").unwrap();
        assert!(repo.publish(&base, &patch).await.is_err());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/file")).unwrap(),
            "human edit"
        );
        std::fs::write(tmp.path().join("src/file"), "base").unwrap();
        repo.publish(&base, &patch).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/file")).unwrap(),
            "verified"
        );
        assert_eq!(head(tmp.path()).await.unwrap(), base); // No commit in user checkout.
        let start = std::time::Instant::now();
        assert!(
            repo.checks(&repo.integration, &["sleep 5".into()], 1)
                .await
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
