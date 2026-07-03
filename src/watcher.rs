use crate::activity::FileChangeKind;

/// Parse `git status --porcelain -z` output into change kinds and
/// repo-relative paths. In -z format entries are NUL-terminated
/// `XY <path>`, and rename/copy entries are followed by the original
/// path as an extra NUL-terminated token.
#[allow(dead_code)] // consumed by collect_activity in Task 5
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

#[cfg(test)]
mod tests {
    use super::*;

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
