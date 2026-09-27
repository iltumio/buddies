//! Bounded blocking executor for redb. Permits live inside jobs even if callers cancel.
use crate::activity::FileActivityEntry;
use crate::memory::{MemoryEntry, SearchFilters};
use crate::skill::{SkillEntry, SkillSearchFilters, SkillSearchResult, SkillVote};
use crate::storage::Storage;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Semaphore;
#[cfg(test)]
use uuid::Uuid;

pub struct AsyncStorage {
    inner: Arc<Storage>,
    permits: Arc<Semaphore>,
}

impl AsyncStorage {
    pub async fn open(dir: Option<std::path::PathBuf>) -> Result<Self> {
        let inner = tokio::task::spawn_blocking(move || -> Result<Storage> {
            if let Some(dir) = dir {
                std::fs::create_dir_all(&dir)?;
                Storage::open(&dir.join("buddies.redb"))
            } else {
                Storage::in_memory()
            }
        })
        .await??;
        Ok(Self {
            inner: Arc::new(inner),
            permits: Arc::new(Semaphore::new(4)),
        })
    }

    async fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce(&Storage) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self.permits.clone().acquire_owned().await?;
        let storage = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            job(&storage)
        })
        .await?
    }
    pub async fn store(&self, entry: &MemoryEntry) -> Result<()> {
        let entry = entry.clone();
        self.run(move |storage| storage.store(&entry)).await
    }
    #[cfg(test)]
    pub async fn get(&self, id: Uuid) -> Result<Option<MemoryEntry>> {
        self.run(move |storage| storage.get(id)).await
    }
    pub async fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let query = query.to_owned();
        let filters = filters.clone();
        self.run(move |storage| storage.search(&query, &filters, limit))
            .await
    }
    pub async fn list(&self, filters: &SearchFilters, limit: usize) -> Result<Vec<MemoryEntry>> {
        let filters = filters.clone();
        self.run(move |storage| storage.list(&filters, limit)).await
    }
    pub async fn store_skill(&self, entry: &SkillEntry) -> Result<()> {
        let entry = entry.clone();
        self.run(move |storage| storage.store_skill(&entry)).await
    }
    pub async fn get_skill(&self, hash: &str) -> Result<Option<SkillEntry>> {
        let hash = hash.to_owned();
        self.run(move |storage| storage.get_skill(&hash)).await
    }
    pub async fn vote_skill(&self, vote: &SkillVote) -> Result<()> {
        let vote = vote.clone();
        self.run(move |storage| storage.vote_skill(&vote)).await
    }
    pub async fn get_skill_rank(&self, hash: &str) -> Result<i64> {
        let hash = hash.to_owned();
        self.run(move |storage| storage.get_skill_rank(&hash)).await
    }
    pub async fn search_skills(
        &self,
        query: &str,
        filters: &SkillSearchFilters,
        limit: usize,
    ) -> Result<Vec<SkillSearchResult>> {
        let query = query.to_owned();
        let filters = filters.clone();
        self.run(move |storage| storage.search_skills(&query, &filters, limit))
            .await
    }
    pub async fn store_file_activity(&self, entry: &FileActivityEntry, now: u64) -> Result<()> {
        let entry = entry.clone();
        self.run(move |storage| storage.store_file_activity(&entry, now))
            .await
    }
    pub async fn get_file_activity(
        &self,
        repo: &str,
        paths: Option<&[String]>,
        now: u64,
    ) -> Result<Vec<FileActivityEntry>> {
        let repo = repo.to_owned();
        let paths = paths.map(<[String]>::to_vec);
        self.run(move |storage| storage.get_file_activity(&repo, paths.as_deref(), now))
            .await
    }
    pub async fn get_peer_file_activity(
        &self,
        repo: &str,
        path: &str,
        peer: &str,
        now: u64,
    ) -> Result<Option<FileActivityEntry>> {
        let repo = repo.to_owned();
        let path = path.to_owned();
        let peer = peer.to_owned();
        self.run(move |storage| storage.get_peer_file_activity(&repo, &path, &peer, now))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_jobs_keep_their_permit_and_do_not_block_the_runtime() {
        let storage = Arc::new(AsyncStorage::open(None).await.unwrap());
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let mut jobs = Vec::new();
        for _ in 0..5 {
            let storage = storage.clone();
            let started = started.clone();
            let release = release.clone();
            jobs.push(tokio::spawn(async move {
                storage
                    .run(move |_| {
                        started.fetch_add(1, Ordering::SeqCst);
                        let (lock, cv) = &*release;
                        let mut done = lock.lock().unwrap();
                        while !*done {
                            done = cv.wait(done).unwrap();
                        }
                        Ok(())
                    })
                    .await
            }));
        }
        let ready = tokio::time::timeout(Duration::from_secs(2), async {
            while started.load(Ordering::SeqCst) < 4 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        // Release even if the assertion fails, so the runtime can drain blocking jobs.
        for job in &jobs {
            job.abort();
        }
        for job in jobs {
            let _ = job.await;
        }
        let count = started.load(Ordering::SeqCst);
        *release.0.lock().unwrap() = true;
        release.1.notify_all();
        assert!(
            ready.is_ok(),
            "runtime kept responding while redb worker slots were busy"
        );
        assert_eq!(
            count, 4,
            "cancelled callers must not start more than four blocking jobs"
        );
    }
}
