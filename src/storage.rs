use std::path::Path;

use anyhow::Result;
use redb::backends::InMemoryBackend;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use uuid::Uuid;

use crate::activity::FileActivityEntry;
use crate::memory::{MemoryEntry, SearchFilters};
use crate::skill::{SkillEntry, SkillSearchFilters, SkillSearchResult, SkillVote};

const ROOM_TICKETS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("room_tickets");
const NODE_SETTINGS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("node_settings");

const MEMORIES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("memories");
const SKILLS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("skills");
const SKILL_VOTES_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("verified_skill_votes_v2");
const FILE_ACTIVITY_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("file_activity");

/// Peer file activity older than this is pruned/ignored.
pub const FILE_ACTIVITY_TTL_SECS: u64 = 86_400;

fn activity_key(repo: &str, path: &str, peer: &str) -> String {
    // NUL separators: valid in Rust strings. Peer-supplied fields CAN carry
    // NUL bytes, so the FileActivity handler rejects them before storage
    // (see FileActivityEntry::validate_received) to prevent key aliasing.
    format!("{repo}\u{0}{path}\u{0}{peer}")
}

pub struct Storage {
    db: Database,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path)?;
        let tx = db.begin_write()?;
        {
            let _ = tx.open_table(ROOM_TICKETS_TABLE)?;
            let _ = tx.open_table(NODE_SETTINGS_TABLE)?;
            let _ = tx.open_table(MEMORIES_TABLE)?;
            let _ = tx.open_table(SKILLS_TABLE)?;
            let _ = tx.open_table(SKILL_VOTES_TABLE)?;
            let _ = tx.open_table(FILE_ACTIVITY_TABLE)?;
        }
        tx.commit()?;
        Ok(Self { db })
    }

    pub fn in_memory() -> Result<Self> {
        let db = Database::builder().create_with_backend(InMemoryBackend::new())?;
        let tx = db.begin_write()?;
        {
            let _ = tx.open_table(ROOM_TICKETS_TABLE)?;
            let _ = tx.open_table(NODE_SETTINGS_TABLE)?;
            let _ = tx.open_table(MEMORIES_TABLE)?;
            let _ = tx.open_table(SKILLS_TABLE)?;
            let _ = tx.open_table(SKILL_VOTES_TABLE)?;
            let _ = tx.open_table(FILE_ACTIVITY_TABLE)?;
        }
        tx.commit()?;
        Ok(Self { db })
    }

    /// Commit a durable queue operation atomically.
    pub fn worker_queue<T>(
        &self,
        operation: impl FnOnce(&mut crate::worker_queue::Queue) -> Result<T>,
    ) -> Result<T> {
        const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("worker_queue");
        let tx = self.db.begin_write()?;
        let result = {
            let mut table = tx.open_table(TABLE)?;
            let previous = table.get("state")?.map(|v| v.value().to_vec());
            let mut queue = match &previous {
                Some(value) => serde_json::from_slice(value)?,
                None => crate::worker_queue::Queue::default(),
            };
            let result = operation(&mut queue)?;
            let bytes = serde_json::to_vec(&queue)?;
            if previous.as_deref() != Some(bytes.as_slice()) {
                table.insert("state", bytes.as_slice())?;
            }
            result
        };
        tx.commit()?;
        Ok(result)
    }

    /// Create the transport identity once, atomically with its persistence.
    pub fn endpoint_secret(&self) -> Result<iroh::SecretKey> {
        let tx = self.db.begin_write()?;
        let secret = {
            let mut table = tx.open_table(NODE_SETTINGS_TABLE)?;
            let existing = table.get("endpoint_secret")?.map(|v| v.value().to_vec());
            if let Some(bytes) = existing {
                let bytes: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("invalid stored endpoint key"))?;
                iroh::SecretKey::from_bytes(&bytes)
            } else {
                let secret = iroh::SecretKey::generate();
                table.insert("endpoint_secret", secret.to_bytes().as_slice())?;
                secret
            }
        };
        tx.commit()?;
        Ok(secret)
    }

    pub fn room_ticket(&self, room: &str) -> Result<Option<crate::ticket::RoomTicket>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(ROOM_TICKETS_TABLE)?;
        table
            .get(room)?
            .map(|value| value.value().parse())
            .transpose()
    }

    pub fn save_room_ticket(&self, ticket: &crate::ticket::RoomTicket) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(ROOM_TICKETS_TABLE)?;
            table.insert(ticket.room.as_str(), ticket.to_string().as_str())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn store(&self, entry: &MemoryEntry) -> Result<()> {
        let key = entry.id.to_string();
        let value = postcard::to_allocvec(entry)?;
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(MEMORIES_TABLE)?;
            if let Some(existing) = table.get(key.as_str())? {
                let existing: MemoryEntry = postcard::from_bytes(existing.value())?;
                anyhow::ensure!(
                    existing.room == entry.room,
                    "memory id belongs to another room"
                );
            }
            table.insert(key.as_str(), value.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get(&self, id: Uuid) -> Result<Option<MemoryEntry>> {
        let key = id.to_string();
        let tx = self.db.begin_read()?;
        let table = tx.open_table(MEMORIES_TABLE)?;
        match table.get(key.as_str())? {
            Some(value) => {
                let entry: MemoryEntry = postcard::from_bytes(value.value())?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    pub fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(MEMORIES_TABLE)?;
        let mut results = Vec::new();

        let iter = table.iter()?;
        for item in iter {
            let (_key, value) = item?;
            let entry: MemoryEntry = postcard::from_bytes(value.value())?;
            if entry.matches_filters(filters) && (query.is_empty() || entry.matches_query(query)) {
                results.push(entry);
            }
        }

        results.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
        results.truncate(limit);
        Ok(results)
    }

    pub fn list(&self, filters: &SearchFilters, limit: usize) -> Result<Vec<MemoryEntry>> {
        self.search("", filters, limit)
    }

    #[allow(dead_code)]
    pub fn delete(&self, id: Uuid) -> Result<bool> {
        let key = id.to_string();
        let tx = self.db.begin_write()?;
        let removed = {
            let mut table = tx.open_table(MEMORIES_TABLE)?;
            table.remove(key.as_str())?.is_some()
        };
        tx.commit()?;
        Ok(removed)
    }

    pub fn store_skill(&self, entry: &SkillEntry) -> Result<()> {
        let value = postcard::to_allocvec(entry)?;
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(SKILLS_TABLE)?;
            if let Some(existing) = table.get(entry.hash.as_str())? {
                let existing: SkillEntry = postcard::from_bytes(existing.value())?;
                anyhow::ensure!(
                    existing.room == entry.room,
                    "skill hash belongs to another room"
                );
            }
            table.insert(entry.hash.as_str(), value.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_skill(&self, hash: &str) -> Result<Option<SkillEntry>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(SKILLS_TABLE)?;
        match table.get(hash)? {
            Some(value) => {
                let entry: SkillEntry = postcard::from_bytes(value.value())?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    pub fn vote_skill(&self, vote: &SkillVote) -> Result<()> {
        if vote.score != 1 && vote.score != -1 {
            anyhow::bail!("invalid vote score {}: must be 1 or -1", vote.score);
        }
        let key = format!("{}:{}", vote.skill_hash, vote.voter);
        let value = postcard::to_allocvec(vote)?;
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(SKILL_VOTES_TABLE)?;
            table.insert(key.as_str(), value.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_skill_rank(&self, skill_hash: &str) -> Result<i64> {
        let prefix = format!("{skill_hash}:");
        let tx = self.db.begin_read()?;
        let table = tx.open_table(SKILL_VOTES_TABLE)?;
        let mut rank: i64 = 0;
        for item in table.range(prefix.as_str()..)? {
            let (key, value) = item?;
            if !key.value().starts_with(&prefix) {
                break;
            }
            let vote: SkillVote = postcard::from_bytes(value.value())?;
            rank += vote.score as i64;
        }
        Ok(rank)
    }

    pub fn search_skills(
        &self,
        query: &str,
        filters: &SkillSearchFilters,
        limit: usize,
    ) -> Result<Vec<SkillSearchResult>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(SKILLS_TABLE)?;
        let mut candidates = Vec::new();

        for item in table.iter()? {
            let (_key, value) = item?;
            let entry: SkillEntry = postcard::from_bytes(value.value())?;
            if entry.matches_filters(filters) && (query.is_empty() || entry.matches_query(query)) {
                candidates.push(entry);
            }
        }
        // Aggregate all votes once in the same read snapshot as the skills.
        let votes = tx.open_table(SKILL_VOTES_TABLE)?;
        let mut ranks = std::collections::HashMap::<String, i64>::new();
        for item in votes.iter()? {
            let (_, value) = item?;
            let vote: SkillVote = postcard::from_bytes(value.value())?;
            *ranks.entry(vote.skill_hash).or_default() += i64::from(vote.score);
        }
        let mut results: Vec<SkillSearchResult> = candidates
            .into_iter()
            .map(|entry| {
                let rank = ranks.get(&entry.hash).copied().unwrap_or(0);
                SkillSearchResult { entry, rank }
            })
            .collect();

        results.sort_by(|a, b| {
            b.rank
                .cmp(&a.rank)
                .then(b.entry.timestamp.cmp(&a.entry.timestamp))
        });
        results.truncate(limit);
        Ok(results)
    }

    pub fn store_file_activity(&self, entry: &FileActivityEntry, now: u64) -> Result<()> {
        let key = activity_key(&entry.repo, &entry.path, &entry.author);
        let value = postcard::to_allocvec(entry)?;
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(FILE_ACTIVITY_TABLE)?;
            table.insert(key.as_str(), value.as_slice())?;

            // lazy TTL prune: collect stale keys, then remove
            let mut stale = Vec::new();
            for item in table.iter()? {
                let (k, v) = item?;
                let e: FileActivityEntry = postcard::from_bytes(v.value())?;
                if e.is_expired(now, FILE_ACTIVITY_TTL_SECS) {
                    stale.push(k.value().to_string());
                }
            }
            for k in stale {
                table.remove(k.as_str())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_file_activity(
        &self,
        repo: &str,
        paths: Option<&[String]>,
        now: u64,
    ) -> Result<Vec<FileActivityEntry>> {
        let prefix = format!("{repo}\u{0}");
        let tx = self.db.begin_read()?;
        let table = tx.open_table(FILE_ACTIVITY_TABLE)?;
        let mut results = Vec::new();
        for item in table.iter()? {
            let (k, v) = item?;
            if !k.value().starts_with(&prefix) {
                continue;
            }
            let entry: FileActivityEntry = postcard::from_bytes(v.value())?;
            if entry.is_expired(now, FILE_ACTIVITY_TTL_SECS) {
                continue;
            }
            if let Some(paths) = paths
                && !paths.contains(&entry.path)
            {
                continue;
            }
            results.push(entry);
        }
        results.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
        Ok(results)
    }

    pub fn get_peer_file_activity(
        &self,
        repo: &str,
        path: &str,
        peer: &str,
        now: u64,
    ) -> Result<Option<FileActivityEntry>> {
        let key = activity_key(repo, path, peer);
        let tx = self.db.begin_read()?;
        let table = tx.open_table(FILE_ACTIVITY_TABLE)?;
        match table.get(key.as_str())? {
            Some(value) => {
                let entry: FileActivityEntry = postcard::from_bytes(value.value())?;
                if entry.is_expired(now, FILE_ACTIVITY_TTL_SECS) {
                    return Ok(None);
                }
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::FILE_ACTIVITY_TTL_SECS;
    use super::Storage;
    use crate::activity::{FileActivityEntry, FileChangeKind};
    use crate::memory::{MemoryEntry, MemoryKind, SearchFilters};
    use crate::skill::SkillVote;
    use uuid::Uuid;

    fn entry(
        room: &str,
        title: &str,
        content: &str,
        kind: MemoryKind,
        tags: Vec<&str>,
        timestamp: u64,
    ) -> MemoryEntry {
        MemoryEntry {
            id: Uuid::new_v4(),
            author: "tester".to_string(),
            timestamp,
            room: room.to_string(),
            kind,
            title: title.to_string(),
            content: content.to_string(),
            tags: tags.into_iter().map(ToString::to_string).collect(),
            references: vec![],
        }
    }

    fn test_storage() -> Storage {
        let dir = std::env::temp_dir().join(format!("buddies-storage-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create test dir");
        Storage::open(&dir.join("buddies.redb")).expect("storage init")
    }

    fn activity(repo: &str, path: &str, peer: &str, timestamp: u64) -> FileActivityEntry {
        FileActivityEntry {
            repo: repo.to_string(),
            branch: "main".to_string(),
            path: path.to_string(),
            kind: FileChangeKind::Changed,
            diff: "+line\n".to_string(),
            content_hash: "abc".to_string(),
            author: peer.to_string(),
            timestamp,
        }
    }

    #[test]
    fn file_activity_stores_latest_per_peer_and_prunes_stale() {
        let storage = test_storage();
        let now = 1_000_000;

        storage
            .store_file_activity(&activity("repo-a", "src/a.rs", "alice", now - 10), now)
            .expect("store alice v1");
        // same key overwrites (last writer wins)
        storage
            .store_file_activity(&activity("repo-a", "src/a.rs", "alice", now - 5), now)
            .expect("store alice v2");
        storage
            .store_file_activity(&activity("repo-a", "src/a.rs", "bob", now - 3), now)
            .expect("store bob");
        storage
            .store_file_activity(&activity("repo-a", "src/b.rs", "bob", now - 2), now)
            .expect("store bob other file");
        storage
            .store_file_activity(&activity("repo-b", "src/a.rs", "carol", now - 1), now)
            .expect("store other repo");

        let all = storage
            .get_file_activity("repo-a", None, now)
            .expect("query repo-a");
        assert_eq!(all.len(), 3);

        let filtered = storage
            .get_file_activity("repo-a", Some(&["src/a.rs".to_string()]), now)
            .expect("query one path");
        assert_eq!(filtered.len(), 2);

        let alice = storage
            .get_peer_file_activity("repo-a", "src/a.rs", "alice", now)
            .expect("query alice")
            .expect("alice entry exists");
        assert_eq!(alice.timestamp, now - 5); // overwritten, not duplicated

        assert!(
            storage
                .get_peer_file_activity("repo-a", "src/a.rs", "nobody", now)
                .expect("query missing")
                .is_none()
        );

        // stale entries are filtered on read and pruned on the next write
        let later = now + FILE_ACTIVITY_TTL_SECS + 100;
        assert!(
            storage
                .get_file_activity("repo-a", None, later)
                .expect("query later")
                .is_empty()
        );
        storage
            .store_file_activity(&activity("repo-a", "src/c.rs", "dave", later), later)
            .expect("store triggers prune");
        let remaining = storage
            .get_file_activity("repo-a", None, later)
            .expect("query after prune");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].path, "src/c.rs");
    }

    #[test]
    fn file_activity_ttl_arithmetic_survives_extreme_timestamps() {
        // Defense in depth: the room layer rejects far-future timestamps,
        // but the TTL math itself must never overflow (panics in debug).
        let storage = test_storage();
        let now = 1_000_000;

        storage
            .store_file_activity(&activity("repo-a", "src/a.rs", "mallory", u64::MAX), now)
            .expect("store extreme timestamp");
        storage
            .store_file_activity(&activity("repo-a", "src/b.rs", "alice", now), now)
            .expect("store triggers prune scan over extreme entry");

        let all = storage
            .get_file_activity("repo-a", None, now)
            .expect("query with extreme entry present");
        assert_eq!(all.len(), 2);
        assert!(
            storage
                .get_peer_file_activity("repo-a", "src/a.rs", "mallory", now)
                .expect("query extreme entry")
                .is_some()
        );
    }

    #[test]
    fn list_returns_descending_timestamp_order() {
        let storage = test_storage();

        let older = entry(
            "room-a",
            "older",
            "first",
            MemoryKind::Context,
            vec!["x"],
            1,
        );
        let newer = entry(
            "room-a",
            "newer",
            "second",
            MemoryKind::Context,
            vec!["x"],
            2,
        );

        storage.store(&older).expect("store older");
        storage.store(&newer).expect("store newer");

        let filters = SearchFilters {
            room: Some("room-a".to_string()),
            kind: None,
            tags: None,
        };

        let results = storage.list(&filters, 10).expect("list results");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "newer");
        assert_eq!(results[1].title, "older");
    }

    #[test]
    fn vote_skill_rejects_scores_other_than_plus_or_minus_one() {
        let storage = test_storage();

        let vote = |score: i8| SkillVote {
            skill_hash: "abc".to_string(),
            voter: "mallory".to_string(),
            score,
            timestamp: 1,
        };

        assert!(storage.vote_skill(&vote(1)).is_ok());
        assert!(storage.vote_skill(&vote(-1)).is_ok());
        assert!(storage.vote_skill(&vote(127)).is_err());
        assert!(storage.vote_skill(&vote(0)).is_err());

        // The invalid votes must not have affected the rank.
        assert_eq!(storage.get_skill_rank("abc").expect("rank"), -1);
    }

    #[test]
    fn search_keeps_newest_entries_when_matches_exceed_limit() {
        let storage = test_storage();

        // Key order (UUID string) is the inverse of timestamp order, so
        // truncating during the scan would drop the newest entry.
        let ids_and_timestamps = [
            ("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", 1, "oldest"),
            ("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", 2, "middle"),
            ("cccccccc-cccc-4ccc-8ccc-cccccccccccc", 3, "newest"),
        ];
        for (id, ts, title) in ids_and_timestamps {
            let mut e = entry("room-a", title, "content", MemoryKind::Context, vec![], ts);
            e.id = Uuid::parse_str(id).expect("valid uuid");
            storage.store(&e).expect("store entry");
        }

        let filters = SearchFilters::default();
        let results = storage.search("", &filters, 2).expect("search");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "newest");
        assert_eq!(results[1].title, "middle");
    }

    #[test]
    fn search_applies_query_and_filters() {
        let storage = test_storage();

        let decision = entry(
            "room-a",
            "db decision",
            "Use postgres",
            MemoryKind::Decision,
            vec!["db", "schema"],
            10,
        );
        let status = entry(
            "room-a",
            "progress",
            "Auth module done",
            MemoryKind::Status,
            vec!["auth"],
            11,
        );

        storage.store(&decision).expect("store decision");
        storage.store(&status).expect("store status");

        let filters = SearchFilters {
            room: Some("room-a".to_string()),
            kind: Some("decision".to_string()),
            tags: Some(vec!["schema".to_string()]),
        };

        let matches = storage.search("postgres", &filters, 10).expect("search");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].title, "db decision");
        assert_eq!(matches[0].kind.to_string(), "decision");
    }
    #[test]
    fn ranking_dataset_regression() {
        use crate::skill::{SkillEntry, SkillSearchFilters, skill_content_hash};
        let storage = Storage::in_memory().unwrap();
        for i in 0..200 {
            let title = format!("skill-{i}");
            let skill = SkillEntry {
                hash: skill_content_hash(&title, "body", &[]),
                title,
                content: "body".into(),
                room: "bench".into(),
                author: "test".into(),
                timestamp: i,
                tags: vec![],
                version: 1,
                parent_hash: None,
                signed_by: None,
                signature: None,
            };
            storage.store_skill(&skill).unwrap();
            for voter in 0..20 {
                storage
                    .vote_skill(&SkillVote {
                        skill_hash: skill.hash.clone(),
                        voter: format!("gpg:{voter}"),
                        score: if i == 199 { 1 } else { -1 },
                        timestamp: 1,
                    })
                    .unwrap();
            }
        }
        let start = std::time::Instant::now();
        let results = storage
            .search_skills("", &SkillSearchFilters::default(), 50)
            .unwrap();
        eprintln!("ranking: 200 skills / 4000 votes: {:?}", start.elapsed());
        assert_eq!(results.len(), 50);
        assert_eq!(results[0].entry.title, "skill-199");
        assert_eq!(results[0].rank, 20);
        assert_eq!(results[1].rank, -20);
    }
    #[test]
    fn legacy_unverified_votes_do_not_enter_verified_ranking() {
        use redb::{ReadableDatabase, TableDefinition};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let storage = Storage::open(&path).unwrap();
        let vote = SkillVote {
            skill_hash: "hash".into(),
            voter: "forged".into(),
            score: 1,
            timestamp: 0,
        };
        let tx = storage.db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(TableDefinition::<&str, &[u8]>::new("skill_votes"))
                .unwrap();
            let bytes = postcard::to_allocvec(&vote).unwrap();
            table.insert("hash:forged", bytes.as_slice()).unwrap();
        }
        tx.commit().unwrap();
        drop(storage);
        let storage = Storage::open(&path).unwrap();
        assert_eq!(storage.get_skill_rank("hash").unwrap(), 0);
        // Old data remains intact for inspection; it is not promoted to trusted data.
        let tx = storage.db.begin_read().unwrap();
        let table = tx
            .open_table(TableDefinition::<&str, &[u8]>::new("skill_votes"))
            .unwrap();
        assert!(table.get("hash:forged").unwrap().is_some());
    }

    #[test]
    fn skill_search_propagates_corrupt_vote_errors() {
        let storage = Storage::in_memory().unwrap();
        let tx = storage.db.begin_write().unwrap();
        {
            let mut table = tx.open_table(super::SKILL_VOTES_TABLE).unwrap();
            table.insert("hash:voter", &[255u8][..]).unwrap();
        }
        tx.commit().unwrap();
        assert!(
            storage
                .search_skills("", &crate::skill::SkillSearchFilters::default(), 50)
                .is_err()
        );
    }
}
