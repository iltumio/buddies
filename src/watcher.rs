use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::Watcher;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::activity::{DirtySet, FileActivityEntry, FileChangeKind, truncate_diff};
use crate::protocol::{P2PMessage, P2PMessageBody};
use crate::room::{RoomManager, now_unix};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScanFingerprint {
    kind: FileChangeKind,
    content_hash: String,
}

fn should_scan_event(event: &notify::Event, repo_root: &Path) -> bool {
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return false;
    }
    event.paths.iter().any(|path| {
        path != repo_root
            && !path
                .components()
                .any(|component| component.as_os_str() == ".git")
    })
}

/// Parse `git status --porcelain -z` output into change kinds and
/// repo-relative paths. In -z format entries are NUL-terminated
/// `XY <path>`, and rename/copy entries are followed by the original
/// path as an extra NUL-terminated token.
pub(crate) fn parse_porcelain_z(bytes: &[u8]) -> Vec<(FileChangeKind, String)> {
    let mut out = Vec::new();
    let mut tokens = bytes
        .split(|b| *b == 0)
        .filter(|t| !t.is_empty())
        .map(|t| String::from_utf8_lossy(t).into_owned());

    while let Some(entry) = tokens.next() {
        if entry.len() < 4 {
            continue;
        }
        let (status, path) = entry.split_at(3);
        let x = status.as_bytes()[0] as char;
        let y = status.as_bytes()[1] as char;
        let path = path.to_string();

        if x == 'R' || x == 'C' {
            if let Some(original) = tokens.next() {
                out.push((FileChangeKind::Deleted, original));
            }
            out.push((FileChangeKind::Created, path));
            continue;
        }

        let kind = if x == '?' || x == 'A' {
            FileChangeKind::Created
        } else if x == 'D' || y == 'D' {
            FileChangeKind::Deleted
        } else {
            FileChangeKind::Changed
        };
        out.push((kind, path));
    }
    out
}

async fn git(repo: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await?)
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read changed file {}", path.display()))?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    Ok(data_encoding::HEXLOWER.encode(&digest))
}

async fn diff_for(repo: &Path, kind: FileChangeKind, path: &str) -> Result<String> {
    // Tracked files (modified/deleted/staged) diff against HEAD; untracked
    // files need --no-index against /dev/null (git exits 1 when files
    // differ there, so ignore the exit code and take stdout).
    let out = git(repo, &["diff", "HEAD", "--", path])
        .await
        .with_context(|| format!("failed to run git diff for {path}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git diff failed for {path}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if !out.stdout.is_empty() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    if kind == FileChangeKind::Created {
        let out = git(repo, &["diff", "--no-index", "--", "/dev/null", path])
            .await
            .with_context(|| format!("failed to run git no-index diff for {path}"))?;
        if !out.status.success() && out.status.code() != Some(1) {
            anyhow::bail!(
                "git no-index diff failed for {path}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Ok(String::new())
}

/// Scan a repo for uncommitted changes. Returns the entries that changed
/// since the previous scan (tracked via `last`) and
/// the full set of currently dirty paths.
async fn collect_activity(
    repo_path: &Path,
    repo_name: &str,
    author: &str,
    last: &mut HashMap<String, ScanFingerprint>,
) -> Result<(Vec<FileActivityEntry>, HashSet<String>)> {
    // --untracked-files=all: without it a new directory shows up as a single
    // `?? newdir/` entry and the files inside are never reported.
    let status = git(
        repo_path,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    )
    .await
    .with_context(|| format!("failed to run git status for {repo_name}"))?;
    if !status.status.success() {
        anyhow::bail!(
            "git status failed for {repo_name}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    let changes = parse_porcelain_z(&status.stdout);
    let dirty: HashSet<String> = changes.iter().map(|(_, p)| p.clone()).collect();
    last.retain(|path, _| dirty.contains(path));

    let branch = git(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .with_context(|| format!("failed to resolve branch for {repo_name}"))?;
    if !branch.status.success() {
        anyhow::bail!(
            "git rev-parse failed for {repo_name}: {}",
            String::from_utf8_lossy(&branch.stderr)
        );
    }
    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();

    let now = now_unix();
    let mut entries = Vec::new();
    for (kind, path) in changes {
        let content_hash = if kind == FileChangeKind::Deleted {
            String::new()
        } else {
            hash_file(&repo_path.join(&path))?
        };
        let fingerprint = ScanFingerprint {
            kind,
            content_hash: content_hash.clone(),
        };
        if last.get(&path) == Some(&fingerprint) {
            continue;
        }
        let diff = truncate_diff(diff_for(repo_path, kind, &path).await?);
        last.insert(path.clone(), fingerprint);
        entries.push(FileActivityEntry {
            repo: repo_name.to_string(),
            branch: branch.clone(),
            path,
            kind,
            diff,
            content_hash,
            author: author.to_string(),
            timestamp: now,
        });
    }
    Ok((entries, dirty))
}

pub struct WatcherManager {
    room_manager: Arc<RoomManager>,
    dirty: Arc<DirtySet>,
    author: String,
    watchers: tokio::sync::Mutex<HashMap<PathBuf, WatchedRepo>>,
}

struct WatchedRepo {
    state: WatchState,
    _watcher: notify::RecommendedWatcher,
    scan_task: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchState {
    pub repo: String,
    pub room: String,
}

impl WatcherManager {
    pub fn new(room_manager: Arc<RoomManager>, dirty: Arc<DirtySet>, author: String) -> Arc<Self> {
        Arc::new(Self {
            room_manager,
            dirty,
            author,
            watchers: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Start watching a git repo, broadcasting file activity to `room`.
    /// Idempotent: watching an already-watched path returns its active state.
    pub async fn watch(
        &self,
        repo_path: &Path,
        room: &str,
        repo_name: Option<String>,
    ) -> Result<WatchState> {
        if !self.room_manager.is_joined(room).await {
            anyhow::bail!("join_room '{room}' before watching a repo into it");
        }
        let repo_path = repo_path
            .canonicalize()
            .with_context(|| format!("cannot resolve repo path {}", repo_path.display()))?;
        if !repo_path.join(".git").exists() {
            anyhow::bail!("not a git repository: {}", repo_path.display());
        }

        let mut watchers = self.watchers.lock().await;
        if let Some(existing) = watchers.get(&repo_path) {
            return Ok(existing.state.clone());
        }

        let repo_name = repo_name.unwrap_or_else(|| {
            repo_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".to_string())
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let watched_root = repo_path.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res
                    && should_scan_event(&event, &watched_root)
                {
                    let _ = tx.send(());
                }
            })?;
        watcher.watch(&repo_path, notify::RecursiveMode::Recursive)?;

        let room_manager = Arc::clone(&self.room_manager);
        let dirty = Arc::clone(&self.dirty);
        let author = self.author.clone();
        let scan_repo_path = repo_path.clone();
        let scan_repo_name = repo_name.clone();
        let scan_room = room.to_string();
        let scan_task = tokio::spawn(async move {
            let mut last: HashMap<String, ScanFingerprint> = HashMap::new();
            let mut reconcile = tokio::time::interval(Duration::from_secs(2));
            reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // `interval` ticks immediately; the explicit initial scan below
            // already covers that first tick.
            reconcile.tick().await;
            // initial scan announces current WIP and seeds the dirty set
            scan_and_broadcast(
                &scan_repo_path,
                &scan_repo_name,
                &author,
                &scan_room,
                &room_manager,
                &dirty,
                &mut last,
            )
            .await;
            loop {
                tokio::select! {
                    event = rx.recv() => {
                        if event.is_none() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await; // debounce
                        while rx.try_recv().is_ok() {}
                    }
                    _ = reconcile.tick() => {}
                }
                scan_and_broadcast(
                    &scan_repo_path,
                    &scan_repo_name,
                    &author,
                    &scan_room,
                    &room_manager,
                    &dirty,
                    &mut last,
                )
                .await;
            }
        });

        let state = WatchState {
            repo: repo_name,
            room: room.to_string(),
        };
        watchers.insert(
            repo_path,
            WatchedRepo {
                state: state.clone(),
                _watcher: watcher,
                scan_task,
            },
        );
        Ok(state)
    }

    pub async fn unwatch(&self, repo_path: &Path) -> Result<bool> {
        let repo_path = repo_path
            .canonicalize()
            .with_context(|| format!("cannot resolve repo path {}", repo_path.display()))?;
        let mut watchers = self.watchers.lock().await;
        match watchers.remove(&repo_path) {
            Some(watched) => {
                watched.scan_task.abort();
                self.dirty.clear_repo(&watched.state.repo);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn scan_and_broadcast(
    repo_path: &Path,
    repo_name: &str,
    author: &str,
    room: &str,
    room_manager: &Arc<RoomManager>,
    dirty: &Arc<DirtySet>,
    last: &mut HashMap<String, ScanFingerprint>,
) {
    let (entries, dirty_paths) = match collect_activity(repo_path, repo_name, author, last).await {
        Ok(scan) => scan,
        Err(error) => {
            warn!(repo = %repo_name, %error, "skipping failed repo scan");
            return;
        }
    };
    dirty.update_repo(repo_name, dirty_paths, entries.iter().cloned());
    for entry in entries {
        let msg = P2PMessage::new(P2PMessageBody::FileActivity { entry });
        if let Err(e) = room_manager.broadcast_to_room(room, msg).await {
            warn!(room = %room, error = %e, "failed to broadcast file activity");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{BuddiesNode, BuddiesNodeConfig};
    use std::collections::HashMap;
    use std::path::Path;

    async fn git_ok(repo: &Path, args: &[&str]) {
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .await
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    async fn git_stdout(repo: &Path, args: &[&str]) -> String {
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .await
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("git output is utf-8")
            .trim()
            .to_string()
    }

    async fn fixture_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("buddies-watch-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        git_ok(&dir, &["init", "-b", "main"]).await;
        git_ok(&dir, &["config", "user.email", "t@example.com"]).await;
        git_ok(&dir, &["config", "user.name", "tester"]).await;
        git_ok(&dir, &["config", "commit.gpgsign", "false"]).await;
        std::fs::write(dir.join("tracked.txt"), "original\n").expect("write tracked");
        git_ok(&dir, &["add", "."]).await;
        git_ok(&dir, &["commit", "-m", "init"]).await;
        dir
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("condition became true before timeout");
    }

    #[tokio::test]
    async fn collect_activity_reports_changes_once() {
        let repo = fixture_repo().await;
        let mut last = HashMap::new();

        // clean repo: nothing to report
        let (entries, dirty) = collect_activity(&repo, "fixture", "alice", &mut last)
            .await
            .expect("scan clean repo");
        assert!(entries.is_empty());
        assert!(dirty.is_empty());

        std::fs::write(repo.join("tracked.txt"), "changed\n").expect("modify");
        std::fs::write(repo.join("brand_new.txt"), "hello\n").expect("create");

        let (entries, dirty) = collect_activity(&repo, "fixture", "alice", &mut last)
            .await
            .expect("scan changes");
        assert_eq!(entries.len(), 2);
        assert_eq!(dirty.len(), 2);

        let changed = entries
            .iter()
            .find(|e| e.path == "tracked.txt")
            .expect("tracked entry");
        assert_eq!(changed.kind, FileChangeKind::Changed);
        assert_eq!(changed.repo, "fixture");
        assert_eq!(changed.branch, "main");
        assert_eq!(changed.author, "alice");
        assert!(
            changed.diff.contains("+changed"),
            "diff was: {}",
            changed.diff
        );
        assert!(!changed.content_hash.is_empty());

        let created = entries
            .iter()
            .find(|e| e.path == "brand_new.txt")
            .expect("created entry");
        assert_eq!(created.kind, FileChangeKind::Created);
        assert!(
            created.diff.contains("+hello"),
            "diff was: {}",
            created.diff
        );

        // second scan with no further edits: dedup, nothing re-broadcast
        let (entries, dirty) = collect_activity(&repo, "fixture", "alice", &mut last)
            .await
            .expect("scan unchanged repo");
        assert!(entries.is_empty());
        assert_eq!(dirty.len(), 2);
    }

    #[tokio::test]
    async fn collect_activity_reports_files_inside_untracked_directories() {
        let repo = fixture_repo().await;
        let mut last = HashMap::new();

        // a brand-new directory: plain `git status` reports it as one
        // `?? newdir/` entry; the file inside must still be broadcast
        std::fs::create_dir_all(repo.join("newdir")).expect("create dir");
        std::fs::write(repo.join("newdir/inner.txt"), "inner content\n").expect("create inner");

        let (entries, dirty) = collect_activity(&repo, "fixture", "alice", &mut last)
            .await
            .expect("scan untracked directory");
        assert_eq!(entries.len(), 1);
        assert!(dirty.contains("newdir/inner.txt"));

        let created = &entries[0];
        assert_eq!(created.path, "newdir/inner.txt");
        assert_eq!(created.kind, FileChangeKind::Created);
        assert!(
            created.diff.contains("+inner content"),
            "diff was: {}",
            created.diff
        );
        assert!(!created.content_hash.is_empty());
    }

    #[tokio::test]
    async fn collect_activity_reports_git_failures_instead_of_clearing_dirty_state() {
        let not_a_repo =
            std::env::temp_dir().join(format!("buddies-not-a-repo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&not_a_repo).expect("create non-repo fixture");
        let mut last = HashMap::new();

        let result = collect_activity(&not_a_repo, "fixture", "alice", &mut last).await;

        assert!(result.is_err(), "git failure must skip the scan batch");
    }

    #[tokio::test]
    async fn repeated_watch_returns_the_room_that_is_actually_active() {
        let repo = fixture_repo().await;
        let node = BuddiesNode::new(BuddiesNodeConfig {
            user_name: "alice".into(),
            agent_name: "codex".into(),
            data_dir: None,
            signer: None,
        })
        .await
        .expect("create local node");
        node.room_manager
            .join_room("room-a", Vec::new())
            .await
            .expect("join first room");
        node.room_manager
            .join_room("room-b", Vec::new())
            .await
            .expect("join second room");

        let initial = node
            .watcher_manager
            .watch(&repo, "room-a", Some("shared-repo".into()))
            .await
            .expect("start watcher");
        let repeated = node
            .watcher_manager
            .watch(&repo, "room-b", Some("other-name".into()))
            .await
            .expect("repeat watcher");

        assert_eq!(initial.repo, "shared-repo");
        assert_eq!(initial.room, "room-a");
        assert_eq!(repeated, initial);

        node.watcher_manager
            .unwatch(&repo)
            .await
            .expect("stop watcher");
        node.shutdown().await.expect("shutdown local node");
    }

    #[tokio::test]
    async fn watcher_debounces_edits_and_clears_dirty_state_after_commit() {
        let repo = fixture_repo().await;
        let node = BuddiesNode::new(BuddiesNodeConfig {
            user_name: "alice".into(),
            agent_name: "codex".into(),
            data_dir: None,
            signer: None,
        })
        .await
        .expect("create local node");
        node.room_manager
            .join_room("room", Vec::new())
            .await
            .expect("join room");
        node.watcher_manager
            .watch(&repo, "room", Some("fixture".into()))
            .await
            .expect("start watcher");

        std::fs::write(repo.join("tracked.txt"), "changed\n").expect("modify tracked file");
        wait_until(|| {
            node.watcher_manager
                .dirty
                .is_dirty("fixture", "tracked.txt")
        })
        .await;

        git_ok(&repo, &["add", "tracked.txt"]).await;
        let tree = git_stdout(&repo, &["write-tree"]).await;
        let parent = git_stdout(&repo, &["rev-parse", "HEAD"]).await;
        let commit = git_stdout(
            &repo,
            &["commit-tree", &tree, "-p", &parent, "-m", "finish change"],
        )
        .await;
        // Let the event caused by staging finish its debounce scan while the
        // staged diff still exists. Updating the prepared commit ref later
        // changes only Git metadata.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            node.watcher_manager
                .dirty
                .is_dirty("fixture", "tracked.txt")
        );
        git_ok(&repo, &["update-ref", "refs/heads/main", &commit]).await;
        wait_until(|| {
            !node
                .watcher_manager
                .dirty
                .is_dirty("fixture", "tracked.txt")
        })
        .await;

        node.watcher_manager
            .unwatch(&repo)
            .await
            .expect("stop watcher");
        node.shutdown().await.expect("shutdown local node");
    }

    #[tokio::test]
    async fn watcher_reconciles_stale_dirty_state_when_an_event_is_missed() {
        let repo = fixture_repo().await;
        let node = BuddiesNode::new(BuddiesNodeConfig {
            user_name: "alice".into(),
            agent_name: "codex".into(),
            data_dir: None,
            signer: None,
        })
        .await
        .expect("create local node");
        node.room_manager
            .join_room("room", Vec::new())
            .await
            .expect("join room");
        node.watcher_manager
            .watch(&repo, "room", Some("fixture".into()))
            .await
            .expect("start watcher");
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Model a coalesced/missed filesystem event: the shared state says
        // dirty, while Git is already clean and no further edit will arrive.
        node.watcher_manager
            .dirty
            .set_repo_dirty("fixture", ["tracked.txt".to_string()].into());
        wait_until(|| {
            !node
                .watcher_manager
                .dirty
                .is_dirty("fixture", "tracked.txt")
        })
        .await;

        node.watcher_manager
            .unwatch(&repo)
            .await
            .expect("stop watcher");
        node.shutdown().await.expect("shutdown local node");
    }

    #[test]
    fn parse_porcelain_z_handles_all_change_kinds() {
        // -z format: "XY path\0", renames: "R  new\0old\0"
        let raw = b" M src/modified.rs\0?? new_untracked.txt\0 D gone.txt\0A  staged_new.rs\0R  renamed_new.rs\0renamed_old.rs\0";
        let parsed = parse_porcelain_z(raw);
        assert_eq!(
            parsed,
            vec![
                (FileChangeKind::Changed, "src/modified.rs".to_string()),
                (FileChangeKind::Created, "new_untracked.txt".to_string()),
                (FileChangeKind::Deleted, "gone.txt".to_string()),
                (FileChangeKind::Created, "staged_new.rs".to_string()),
                (FileChangeKind::Deleted, "renamed_old.rs".to_string()),
                (FileChangeKind::Created, "renamed_new.rs".to_string()),
            ]
        );
    }

    #[test]
    fn parse_porcelain_z_ignores_garbage() {
        assert!(parse_porcelain_z(b"").is_empty());
        assert!(parse_porcelain_z(b"X\0").is_empty()); // too short
    }

    #[test]
    fn watcher_ignores_read_access_and_git_metadata_events() {
        use notify::EventKind;
        use notify::event::AccessKind;

        let repo = Path::new("/tmp/repo");
        let access = notify::Event::new(EventKind::Access(AccessKind::Any))
            .add_path(repo.join("src/lib.rs"));
        let git_change = notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(repo.join(".git/index"));
        let source_change = notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(repo.join("src/lib.rs"));

        assert!(!should_scan_event(&access, repo));
        assert!(!should_scan_event(&git_change, repo));
        assert!(should_scan_event(&source_change, repo));
    }
}
