use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

pub const MAX_DIFF_BYTES: usize = 64 * 1024;
pub const DIFF_TRUNCATION_MARKER: &str = "\n[... diff truncated by buddies ...]";
pub(crate) const MAX_ACTIVITY_FIELD_BYTES: usize = 1024;
pub(crate) const MAX_CONTENT_HASH_BYTES: usize = 128;
pub(crate) const MAX_AFFECTED_FILES: usize = 512;
pub(crate) const MAX_TOUCHED_SYMBOLS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChangeKind {
    Changed,
    Created,
    Deleted,
}

impl std::fmt::Display for FileChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Changed => write!(f, "changed"),
            Self::Created => write!(f, "created"),
            Self::Deleted => write!(f, "deleted"),
        }
    }
}

/// Kind of overlap detected between a peer's file activity and local work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Both agents modified the exact same file.
    #[default]
    Direct,
    /// One agent modified a file whose static blast radius (`affected_files`)
    /// includes a file the other agent is currently modifying.
    Impact,
}

impl std::fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct => write!(f, "direct"),
            Self::Impact => write!(f, "impact"),
        }
    }
}

/// One peer's latest change to one file in a watched repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileActivityEntry {
    pub repo: String,
    pub branch: String,
    /// Optional git worktree identifier or checkout path.
    #[serde(default)]
    pub worktree: Option<String>,
    /// Identifier of the AI agent working on the file (e.g. `codex`, `claude`).
    #[serde(default)]
    pub agent: String,
    /// Optional task identifier the agent is currently executing.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Optional target sprint or milestone where the feature should land.
    #[serde(default)]
    pub sprint: Option<String>,
    /// Repo-relative path, forward slashes.
    pub path: String,
    pub kind: FileChangeKind,
    /// Unified diff vs HEAD, capped at MAX_DIFF_BYTES.
    pub diff: String,
    /// SHA-256 hex of current content, empty for Deleted.
    pub content_hash: String,
    pub author: String,
    pub timestamp: u64,
    /// Exported/public symbols modified or defined in this file.
    #[serde(default)]
    pub touched_symbols: Vec<String>,
    /// Repo-relative paths of files affected by this change (Rust & TypeScript).
    #[serde(default)]
    pub affected_files: Vec<String>,
    /// True when impact lists were capped to fit the wire budget.
    #[serde(default)]
    pub impact_truncated: bool,
}

#[derive(Debug, Clone)]
pub struct ConflictEvent {
    pub local: FileActivityEntry,
    pub peer: FileActivityEntry,
    pub kind: ConflictKind,
}

impl FileActivityEntry {
    /// Bound generated metadata as well as received metadata; never drop the whole diff
    /// merely because its dependency graph is large.
    pub fn cap_impact(&mut self) {
        let mut budget = 96 * 1024;
        for (values, count) in [
            (&mut self.affected_files, MAX_AFFECTED_FILES),
            (&mut self.touched_symbols, MAX_TOUCHED_SYMBOLS),
        ] {
            values.sort();
            values.dedup();
            let original = values.len();
            values.retain(|value| {
                if value.len() > MAX_ACTIVITY_FIELD_BYTES || value.len() + 8 > budget {
                    return false;
                }
                budget -= value.len() + 8;
                true
            });
            values.truncate(count);
            self.impact_truncated |= values.len() != original;
        }
    }

    pub(crate) fn validate_received(
        &self,
        now: u64,
        freshness_window_secs: u64,
    ) -> Result<(), &'static str> {
        if self.timestamp.abs_diff(now) > freshness_window_secs {
            return Err("timestamp outside freshness window");
        }
        for field in [
            &self.repo,
            &self.path,
            &self.author,
            &self.branch,
            &self.agent,
        ] {
            if field.as_bytes().contains(&0) {
                return Err("field contains NUL byte");
            }
            if field.len() > MAX_ACTIVITY_FIELD_BYTES {
                return Err("field exceeds length cap");
            }
        }
        for opt_field in [&self.worktree, &self.task_id, &self.sprint]
            .into_iter()
            .flatten()
        {
            if opt_field.as_bytes().contains(&0) {
                return Err("field contains NUL byte");
            }
            if opt_field.len() > MAX_ACTIVITY_FIELD_BYTES {
                return Err("field exceeds length cap");
            }
        }
        if self.affected_files.len() > MAX_AFFECTED_FILES {
            return Err("affected_files exceeds count cap");
        }
        for path in &self.affected_files {
            if path.as_bytes().contains(&0) || path.len() > MAX_ACTIVITY_FIELD_BYTES {
                return Err("invalid affected_files entry");
            }
        }
        if self.touched_symbols.len() > MAX_TOUCHED_SYMBOLS {
            return Err("touched_symbols exceeds count cap");
        }
        for sym in &self.touched_symbols {
            if sym.as_bytes().contains(&0) || sym.len() > MAX_ACTIVITY_FIELD_BYTES {
                return Err("invalid touched_symbols entry");
            }
        }
        if self.diff.len() > MAX_DIFF_BYTES + DIFF_TRUNCATION_MARKER.len() {
            return Err("diff exceeds length cap");
        }
        if self.content_hash.len() > MAX_CONTENT_HASH_BYTES {
            return Err("content_hash exceeds length cap");
        }
        Ok(())
    }

    pub(crate) fn is_expired(&self, now: u64, ttl_secs: u64) -> bool {
        self.timestamp.saturating_add(ttl_secs) < now
    }
}

/// (added, removed) line counts of a unified diff.
pub fn diff_summary(diff: &str) -> (u64, u64) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

pub fn truncate_diff(mut diff: String) -> String {
    if diff.len() <= MAX_DIFF_BYTES {
        return diff;
    }
    let mut cut = MAX_DIFF_BYTES;
    while !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    diff.truncate(cut);
    diff.push_str(DIFF_TRUNCATION_MARKER);
    diff
}

/// Locally-modified paths per watched repo, shared between the watcher
/// (writer) and the gossip handler (reader, for conflict detection).
#[derive(Default)]
pub struct DirtySet {
    inner: RwLock<HashMap<String, DirtyRepo>>,
}

#[derive(Default)]
struct DirtyRepo {
    paths: HashSet<String>,
    activity: HashMap<String, FileActivityEntry>,
}

impl DirtySet {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub fn set_repo_dirty(&self, repo: &str, paths: HashSet<String>) {
        self.update_repo(repo, paths, std::iter::empty());
    }

    pub fn update_repo(
        &self,
        repo: &str,
        paths: HashSet<String>,
        entries: impl IntoIterator<Item = FileActivityEntry>,
    ) {
        let mut repos = self.inner.write().expect("dirty set lock poisoned");
        if paths.is_empty() {
            repos.remove(repo);
            return;
        }

        let state = repos.entry(repo.to_string()).or_default();
        state.paths = paths;
        state.activity.retain(|path, _| state.paths.contains(path));
        for entry in entries {
            if state.paths.contains(&entry.path) {
                state.activity.insert(entry.path.clone(), entry);
            }
        }
    }

    pub fn clear_repo(&self, repo: &str) {
        self.inner
            .write()
            .expect("dirty set lock poisoned")
            .remove(repo);
    }

    #[cfg(test)]
    pub fn is_dirty(&self, repo: &str, path: &str) -> bool {
        self.inner
            .read()
            .expect("dirty set lock poisoned")
            .get(repo)
            .is_some_and(|state| state.paths.contains(path))
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn get(&self, repo: &str, path: &str) -> Option<FileActivityEntry> {
        self.inner
            .read()
            .expect("dirty set lock poisoned")
            .get(repo)
            .and_then(|state| state.activity.get(path))
            .cloned()
    }

    /// Detect both direct file conflicts and transitive impact conflicts with `peer`.
    pub fn find_conflicts(&self, peer: &FileActivityEntry) -> Vec<ConflictEvent> {
        let repos = self.inner.read().expect("dirty set lock poisoned");
        let Some(state) = repos.get(&peer.repo) else {
            return Vec::new();
        };

        let mut events = Vec::new();
        let mut seen_local_paths = HashSet::new();

        // 1. Direct conflict on the exact path
        if let Some(local) = state.activity.get(&peer.path) {
            seen_local_paths.insert(local.path.clone());
            events.push(ConflictEvent {
                local: local.clone(),
                peer: peer.clone(),
                kind: ConflictKind::Direct,
            });
        }

        // 2. Peer's change affects a file we are currently modifying locally
        for affected in &peer.affected_files {
            if !seen_local_paths.contains(affected)
                && let Some(local) = state.activity.get(affected)
            {
                seen_local_paths.insert(local.path.clone());
                events.push(ConflictEvent {
                    local: local.clone(),
                    peer: peer.clone(),
                    kind: ConflictKind::Impact,
                });
            }
        }

        // 3. A file we modified locally affects the file the peer just modified
        for local in state.activity.values() {
            if !seen_local_paths.contains(&local.path) && local.affected_files.contains(&peer.path)
            {
                seen_local_paths.insert(local.path.clone());
                events.push(ConflictEvent {
                    local: local.clone(),
                    peer: peer.clone(),
                    kind: ConflictKind::Impact,
                });
            }
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_summary_counts_added_and_removed_lines() {
        let diff =
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n-old line\n+new line\n+another\n context\n";
        assert_eq!(diff_summary(diff), (2, 1));
    }

    #[test]
    fn truncate_diff_caps_at_limit_on_char_boundary() {
        let short = "small diff".to_string();
        assert_eq!(truncate_diff(short.clone()), short);

        // multi-byte char straddling the cap must not panic
        let long = "é".repeat(MAX_DIFF_BYTES); // 2 bytes each => 128 KiB
        let out = truncate_diff(long);
        assert!(out.len() <= MAX_DIFF_BYTES + DIFF_TRUNCATION_MARKER.len());
        assert!(out.ends_with(DIFF_TRUNCATION_MARKER));
    }

    #[test]
    fn file_change_kind_display() {
        assert_eq!(FileChangeKind::Changed.to_string(), "changed");
        assert_eq!(FileChangeKind::Created.to_string(), "created");
        assert_eq!(FileChangeKind::Deleted.to_string(), "deleted");
    }

    #[test]
    fn dirty_set_tracks_paths_per_repo() {
        let dirty = DirtySet::new();
        assert!(!dirty.is_dirty("repo-a", "src/a.rs"));

        dirty.set_repo_dirty("repo-a", ["src/a.rs".to_string()].into());
        assert!(dirty.is_dirty("repo-a", "src/a.rs"));
        assert!(!dirty.is_dirty("repo-a", "src/b.rs"));
        assert!(!dirty.is_dirty("repo-b", "src/a.rs"));

        // replace semantics: a new scan result overwrites the old set
        dirty.set_repo_dirty("repo-a", ["src/b.rs".to_string()].into());
        assert!(!dirty.is_dirty("repo-a", "src/a.rs"));
        assert!(dirty.is_dirty("repo-a", "src/b.rs"));

        dirty.clear_repo("repo-a");
        assert!(!dirty.is_dirty("repo-a", "src/b.rs"));
    }

    #[test]
    fn dirty_set_retains_local_metadata_until_a_path_is_clean() {
        let dirty = DirtySet::new();
        let local = FileActivityEntry {
            repo: "repo-a".into(),
            branch: "feature/local".into(),
            worktree: Some("wt-local".into()),
            agent: "codex".into(),
            task_id: Some("TASK-1".into()),
            sprint: Some("Sprint-10".into()),
            path: "src/a.rs".into(),
            kind: FileChangeKind::Changed,
            diff: "+local line\n".into(),
            content_hash: "abc".into(),
            author: "local-agent".into(),
            timestamp: 42,
            touched_symbols: vec!["foo".into()],
            affected_files: vec!["src/b.rs".into()],
            impact_truncated: false,
        };

        dirty.update_repo("repo-a", ["src/a.rs".to_string()].into(), [local.clone()]);
        let stored = dirty
            .get("repo-a", "src/a.rs")
            .expect("local activity metadata");
        assert_eq!(stored.branch, "feature/local");
        assert_eq!(stored.worktree.as_deref(), Some("wt-local"));
        assert_eq!(stored.agent, "codex");
        assert_eq!(stored.task_id.as_deref(), Some("TASK-1"));
        assert_eq!(stored.sprint.as_deref(), Some("Sprint-10"));
        assert_eq!(stored.diff, "+local line\n");
        assert_eq!(stored.timestamp, 42);

        // Impact conflict detection when peer edits src/b.rs (affected by src/a.rs)
        let peer_edit = FileActivityEntry {
            repo: "repo-a".into(),
            branch: "feature/peer".into(),
            worktree: Some("wt-peer".into()),
            agent: "claude".into(),
            task_id: Some("TASK-2".into()),
            sprint: Some("Sprint-10".into()),
            path: "src/b.rs".into(),
            kind: FileChangeKind::Changed,
            diff: "+peer line\n".into(),
            content_hash: "def".into(),
            author: "peer-agent".into(),
            timestamp: 43,
            touched_symbols: vec![],
            affected_files: vec![],
            impact_truncated: false,
        };
        let conflicts = dirty.find_conflicts(&peer_edit);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::Impact);
        assert_eq!(conflicts[0].local.path, "src/a.rs");
        assert_eq!(conflicts[0].peer.path, "src/b.rs");

        // A deduplicated scan emits no new entry, but the file is still dirty.
        dirty.update_repo("repo-a", ["src/a.rs".to_string()].into(), []);
        assert!(dirty.get("repo-a", "src/a.rs").is_some());

        dirty.update_repo("repo-a", HashSet::new(), []);
        assert!(dirty.get("repo-a", "src/a.rs").is_none());
    }

    #[test]
    fn generated_large_impact_keeps_activity_valid_and_marks_truncation() {
        let mut entry:FileActivityEntry=serde_json::from_value(serde_json::json!({"repo":"r","branch":"main","path":"a.ts","kind":"Changed","diff":"+x","content_hash":"abc","author":"alice","timestamp":1000})).unwrap();
        entry.affected_files = (0..2000)
            .map(|i| format!("src/{i}/{}", "x".repeat(500)))
            .collect();
        entry.cap_impact();
        assert!(entry.impact_truncated);
        assert!(!entry.affected_files.is_empty());
        entry.validate_received(1000, 600).unwrap();
        assert!(postcard::to_allocvec(&entry).unwrap().len() < 192 * 1024);
    }
}
