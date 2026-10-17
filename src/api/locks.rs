use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Mutex, OwnedMutexGuard};

type LockMap = HashMap<String, Arc<Mutex<()>>>;

/// One async lock per string key, so two requests with the same
/// idempotency key in this process run one after the other. Entries are
/// removed when nobody holds or waits for them.
#[derive(Clone, Default)]
pub struct KeyLocks {
    map: Arc<StdMutex<LockMap>>,
}

/// Holds the lock for one key until dropped.
pub struct KeyGuard {
    key: String,
    map: Arc<StdMutex<LockMap>>,
    guard: Option<OwnedMutexGuard<()>>,
}

impl KeyLocks {
    pub async fn lock(&self, key: String) -> KeyGuard {
        let mutex = {
            let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(key.clone()).or_default().clone()
        };
        let guard = mutex.lock_owned().await;
        KeyGuard {
            key,
            map: self.map.clone(),
            guard: Some(guard),
        }
    }

    /// Number of keys currently tracked.
    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for KeyGuard {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        // Release the lock while holding the map lock, so nobody can clone
        // the entry between the count check and the removal.
        let guard = self.guard.take();
        let only_us = guard
            .as_ref()
            .is_some_and(|g| Arc::strong_count(OwnedMutexGuard::mutex(g)) == 2);
        drop(guard);
        if only_us {
            map.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn same_key_is_serialised() {
        let locks = KeyLocks::default();
        let g = locks.lock("k".into()).await;
        let l2 = locks.clone();
        let waiter = tokio::spawn(async move { l2.lock("k".into()).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        drop(g);
        let g2 = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        drop(g2);
        assert!(locks.is_empty());
    }

    #[tokio::test]
    async fn different_keys_do_not_block_and_are_cleaned_up() {
        let locks = KeyLocks::default();
        let a = locks.lock("a".into()).await;
        let b = tokio::time::timeout(Duration::from_millis(100), locks.lock("b".into()))
            .await
            .unwrap();
        assert_eq!(locks.len(), 2);
        drop(a);
        drop(b);
        assert!(locks.is_empty());
    }
}
