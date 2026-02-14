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
