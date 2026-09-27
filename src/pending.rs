//! Bounded, room-scoped requests. Registrations remove themselves on cancellation.
use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Result, bail};
use uuid::Uuid;

const MAX_PENDING_REQUESTS: usize = 128;

pub struct PendingRequests<T> {
    entries: Mutex<HashMap<Uuid, (String, T)>>,
}

impl<T> Default for PendingRequests<T> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

pub struct Registration<'a, T> {
    registry: &'a PendingRequests<T>,
    id: Uuid,
}

impl<T> PendingRequests<T> {
    pub fn register(&self, id: Uuid, room: &str, value: T) -> Result<Registration<'_, T>> {
        let mut entries = self.entries.lock().expect("pending requests poisoned");
        if entries.len() >= MAX_PENDING_REQUESTS || entries.contains_key(&id) {
            bail!("too many pending requests or duplicate request id");
        }
        entries.insert(id, (room.to_owned(), value));
        Ok(Registration { registry: self, id })
    }

    pub fn get(&self, id: Uuid, room: &str) -> Option<T>
    where
        T: Clone,
    {
        self.entries
            .lock()
            .expect("pending requests poisoned")
            .get(&id)
            .filter(|(r, _)| r == room)
            .map(|(_, value)| value.clone())
    }

    pub fn remove(&self, id: Uuid, room: &str) -> Option<T> {
        let mut entries = self.entries.lock().expect("pending requests poisoned");
        if entries.get(&id).is_some_and(|(r, _)| r == room) {
            entries.remove(&id).map(|(_, value)| value)
        } else {
            None
        }
    }

    pub fn clear(&self) {
        self.entries
            .lock()
            .expect("pending requests poisoned")
            .clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

impl<T> Drop for Registration<'_, T> {
    fn drop(&mut self) {
        self.registry
            .entries
            .lock()
            .expect("pending requests poisoned")
            .remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_request_releases_registration() {
        let registry = PendingRequests::default();
        let id = Uuid::new_v4();
        let future = async {
            let _registration = registry.register(id, "a", 42).unwrap();
            std::future::pending::<()>().await;
        };
        let mut future = Box::pin(future);
        tokio::select! { biased; _ = &mut future => unreachable!(), _ = tokio::task::yield_now() => {} }
        assert_eq!(registry.get(id, "a"), Some(42));
        assert_eq!(registry.get(id, "b"), None);
        assert_eq!(registry.remove(id, "b"), None);
        drop(future);
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn registry_bounds_concurrent_requests() {
        let registry = PendingRequests::default();
        let guards: Vec<_> = (0..MAX_PENDING_REQUESTS)
            .map(|_| registry.register(Uuid::new_v4(), "a", ()).unwrap())
            .collect();
        assert!(registry.register(Uuid::new_v4(), "a", ()).is_err());
        drop(guards);
        assert_eq!(registry.len(), 0);
    }
}
