#![allow(dead_code)]

use serde::{Deserialize, Serialize};

pub const MAX_DIFF_BYTES: usize = 64 * 1024;
pub const DIFF_TRUNCATION_MARKER: &str = "\n[... diff truncated by buddies ...]";

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

/// One peer's latest change to one file in a watched repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileActivityEntry {
    pub repo: String,
    pub branch: String,
    /// Repo-relative path, forward slashes.
    pub path: String,
    pub kind: FileChangeKind,
    /// Unified diff vs HEAD, capped at MAX_DIFF_BYTES.
    pub diff: String,
    /// SHA-256 hex of current content, empty for Deleted.
    pub content_hash: String,
    pub author: String,
    pub timestamp: u64,
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
}
