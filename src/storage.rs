use std::path::Path;

use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use uuid::Uuid;

use crate::memory::{MemoryEntry, SearchFilters};

const MEMORIES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("memories");

pub struct Storage {
    db: Database,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path)?;
        let tx = db.begin_write()?;
        {
            let _ = tx.open_table(MEMORIES_TABLE)?;
        }
        tx.commit()?;
        Ok(Self { db })
    }

    pub fn in_memory() -> Result<Self> {
        let db = Database::create("")?;
        let tx = db.begin_write()?;
        {
            let _ = tx.open_table(MEMORIES_TABLE)?;
        }
        tx.commit()?;
        Ok(Self { db })
    }

    pub fn store(&self, entry: &MemoryEntry) -> Result<()> {
        let key = entry.id.to_string();
        let value = postcard::to_allocvec(entry)?;
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(MEMORIES_TABLE)?;
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
            if results.len() >= limit {
                break;
            }
        }

        results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
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
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Storage;
    use crate::memory::{MemoryEntry, MemoryKind, SearchFilters};
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
        let dir = std::env::temp_dir().join(format!("smemo-storage-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create test dir");
        Storage::open(&dir.join("smemo.redb")).expect("storage init")
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
}
