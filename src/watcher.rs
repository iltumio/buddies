use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::Watcher;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::activity::{DirtySet, FileActivityEntry, FileChangeKind, truncate_diff};
use crate::protocol::{P2PMessage, P2PMessageBody};
use crate::room::{RoomManager, now_unix};

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

fn hash_file(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            data_encoding::HEXLOWER.encode(&digest)
        }
        Err(_) => String::new(),
    }
}

async fn diff_for(repo: &Path, kind: FileChangeKind, path: &str) -> String {
    // Tracked files (modified/deleted/staged) diff against HEAD; untracked
    // files need --no-index against /dev/null (git exits 1 when files
    // differ there, so ignore the exit code and take stdout).
    if let Ok(out) = git(repo, &["diff", "HEAD", "--", path]).await
        && !out.stdout.is_empty()
    {
        return String::from_utf8_lossy(&out.stdout).into_owned();
    }
    if kind == FileChangeKind::Created
        && let Ok(out) = git(repo, &["diff", "--no-index", "--", "/dev/null", path]).await
    {
        return String::from_utf8_lossy(&out.stdout).into_owned();
    }
    String::new()
}

/// Scan a repo for uncommitted changes. Returns the entries that changed
/// since the previous scan (tracked via `last`: path -> "kind:hash") and
/// the full set of currently dirty paths.
pub(crate) async fn collect_activity(
    repo_path: &Path,
    repo_name: &str,
    author: &str,
    last: &mut HashMap<String, String>,
) -> (Vec<FileActivityEntry>, HashSet<String>) {
    let status = match git(repo_path, &["status", "--porcelain", "-z"]).await {
        Ok(out) if out.status.success() => out.stdout,
        Ok(out) => {
            warn!(repo = %repo_name, stderr = %String::from_utf8_lossy(&out.stderr), "git status failed");
            return (Vec::new(), HashSet::new());
        }
        Err(error) => {
            warn!(repo = %repo_name, %error, "failed to run git status");
            return (Vec::new(), HashSet::new());
        }
    };

    let changes = parse_porcelain_z(&status);
    let dirty: HashSet<String> = changes.iter().map(|(_, p)| p.clone()).collect();
    last.retain(|path, _| dirty.contains(path));

    let branch = match git(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    };

    let now = now_unix();
    let mut entries = Vec::new();
    for (kind, path) in changes {
        let content_hash = if kind == FileChangeKind::Deleted {
            String::new()
        } else {
            hash_file(&repo_path.join(&path))
        };
        let marker = format!("{kind}:{content_hash}");
        if last.get(&path) == Some(&marker) {
            continue;
        }
        let diff = truncate_diff(diff_for(repo_path, kind, &path).await);
        last.insert(path.clone(), marker);
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
    (entries, dirty)
}

pub struct WatcherManager {
    room_manager: Arc<RoomManager>,
    dirty: Arc<DirtySet>,
    author: String,
    watchers: tokio::sync::Mutex<HashMap<PathBuf, WatchedRepo>>,
}

struct WatchedRepo {
    repo_name: String,
    _watcher: notify::RecommendedWatcher,
    scan_task: tokio::task::JoinHandle<()>,
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
    /// Idempotent: watching an already-watched path returns its repo name.
    #[allow(dead_code)] // consumed by MCP tools in Task 7
    pub async fn watch(
        &self,
        repo_path: &Path,
        room: &str,
        repo_name: Option<String>,
    ) -> Result<String> {
        let repo_path = repo_path
            .canonicalize()
            .with_context(|| format!("cannot resolve repo path {}", repo_path.display()))?;
        if !repo_path.join(".git").exists() {
            anyhow::bail!("not a git repository: {}", repo_path.display());
        }

        let mut watchers = self.watchers.lock().await;
        if let Some(existing) = watchers.get(&repo_path) {
            return Ok(existing.repo_name.clone());
        }

        let repo_name = repo_name.unwrap_or_else(|| {
            repo_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".to_string())
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    // .git churn (index, lockfiles) must not trigger scans
                    let outside_git = event
                        .paths
                        .iter()
                        .any(|p| !p.components().any(|c| c.as_os_str() == ".git"));
                    if outside_git {
                        let _ = tx.send(());
                    }
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
            let mut last: HashMap<String, String> = HashMap::new();
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
            while rx.recv().await.is_some() {
                tokio::time::sleep(Duration::from_secs(1)).await; // debounce
                while rx.try_recv().is_ok() {}
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

        watchers.insert(
            repo_path,
            WatchedRepo {
                repo_name: repo_name.clone(),
                _watcher: watcher,
                scan_task,
            },
        );
        Ok(repo_name)
    }

    #[allow(dead_code)] // consumed by MCP tools in Task 7
    pub async fn unwatch(&self, repo_path: &Path) -> Result<bool> {
        let repo_path = repo_path
            .canonicalize()
            .with_context(|| format!("cannot resolve repo path {}", repo_path.display()))?;
        let mut watchers = self.watchers.lock().await;
        match watchers.remove(&repo_path) {
            Some(watched) => {
                watched.scan_task.abort();
                self.dirty.clear_repo(&watched.repo_name);
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
    last: &mut HashMap<String, String>,
) {
    let (entries, dirty_paths) = collect_activity(repo_path, repo_name, author, last).await;
    dirty.set_repo_dirty(repo_name, dirty_paths);
    for entry in entries {
        let msg = P2PMessage::new(P2PMessageBody::FileActivity { entry });
        if let Err(e) = room_manager.broadcast_to_room(room, msg).await {
            debug!(error = %e, "failed to broadcast file activity");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn collect_activity_reports_changes_once() {
        let repo = fixture_repo().await;
        let mut last = HashMap::new();

        // clean repo: nothing to report
        let (entries, dirty) = collect_activity(&repo, "fixture", "alice", &mut last).await;
        assert!(entries.is_empty());
        assert!(dirty.is_empty());

        std::fs::write(repo.join("tracked.txt"), "changed\n").expect("modify");
        std::fs::write(repo.join("brand_new.txt"), "hello\n").expect("create");

        let (entries, dirty) = collect_activity(&repo, "fixture", "alice", &mut last).await;
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
        let (entries, dirty) = collect_activity(&repo, "fixture", "alice", &mut last).await;
        assert!(entries.is_empty());
        assert_eq!(dirty.len(), 2);
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
}
