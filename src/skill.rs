use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A content-addressable skill entry.
///
/// The `hash` field is the hex-encoded SHA-256 of the canonical content
/// (title + content + sorted tags).  Two peers that publish identical skills
/// will produce the same hash, enabling automatic deduplication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEntry {
    pub hash: String,
    pub author: String,
    pub timestamp: u64,
    pub room: String,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub version: u32,
    pub parent_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillVote {
    pub skill_hash: String,
    pub voter: String,
    pub score: i8,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSearchResult {
    pub entry: SkillEntry,
    pub rank: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillSearchFilters {
    pub room: Option<String>,
    pub tags: Option<Vec<String>>,
}

/// Compute the canonical SHA-256 content hash for a skill.
///
/// Tags are sorted before hashing so that tag ordering is irrelevant.
pub fn skill_content_hash(title: &str, content: &str, tags: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"smemo:skill:");
    hasher.update(title.as_bytes());
    hasher.update(b"\0");
    hasher.update(content.as_bytes());
    hasher.update(b"\0");

    let mut sorted_tags: Vec<&str> = tags.iter().map(String::as_str).collect();
    sorted_tags.sort();
    for tag in sorted_tags {
        hasher.update(tag.as_bytes());
        hasher.update(b"\0");
    }

    let hash: [u8; 32] = hasher.finalize().into();
    data_encoding::HEXLOWER.encode(&hash)
}

impl SkillEntry {
    pub fn matches_query(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        self.title.to_lowercase().contains(&q)
            || self.content.to_lowercase().contains(&q)
            || self.tags.iter().any(|t| t.to_lowercase().contains(&q))
    }

    pub fn matches_filters(&self, filters: &SkillSearchFilters) -> bool {
        if let Some(ref room) = filters.room
            && &self.room != room
        {
            return false;
        }
        if let Some(ref tags) = filters.tags {
            let has_any = tags.iter().any(|t| self.tags.contains(t));
            if !has_any && !tags.is_empty() {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_deterministic() {
        let h1 = skill_content_hash("deploy", "run deploy.sh", &["ci".into(), "ops".into()]);
        let h2 = skill_content_hash("deploy", "run deploy.sh", &["ci".into(), "ops".into()]);
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_ignores_tag_order() {
        let h1 = skill_content_hash("x", "y", &["a".into(), "b".into()]);
        let h2 = skill_content_hash("x", "y", &["b".into(), "a".into()]);
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_changes_with_content() {
        let h1 = skill_content_hash("deploy", "run deploy.sh", &[]);
        let h2 = skill_content_hash("deploy", "run deploy-v2.sh", &[]);
        assert_ne!(h1, h2);
    }

    #[test]
    fn matches_query_is_case_insensitive() {
        let entry = SkillEntry {
            hash: String::new(),
            author: String::new(),
            timestamp: 0,
            room: String::new(),
            title: "Deploy to Prod".into(),
            content: "instructions".into(),
            tags: vec!["CI".into()],
            version: 1,
            parent_hash: None,
        };
        assert!(entry.matches_query("deploy"));
        assert!(entry.matches_query("ci"));
        assert!(!entry.matches_query("rollback"));
    }

    #[test]
    fn matches_filters_room_and_tags() {
        let entry = SkillEntry {
            hash: String::new(),
            author: String::new(),
            timestamp: 0,
            room: "team".into(),
            title: String::new(),
            content: String::new(),
            tags: vec!["rust".into(), "deploy".into()],
            version: 1,
            parent_hash: None,
        };

        let room_mismatch = SkillSearchFilters {
            room: Some("other".into()),
            tags: None,
        };
        assert!(!entry.matches_filters(&room_mismatch));

        let tag_match = SkillSearchFilters {
            room: Some("team".into()),
            tags: Some(vec!["deploy".into()]),
        };
        assert!(entry.matches_filters(&tag_match));

        let no_matching_tag = SkillSearchFilters {
            room: None,
            tags: Some(vec!["python".into()]),
        };
        assert!(!entry.matches_filters(&no_matching_tag));
    }
}
