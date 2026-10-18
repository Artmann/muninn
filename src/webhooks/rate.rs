use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Counts calls per key in fixed windows, inside this process only.
#[derive(Clone, Default)]
pub struct RateLimiter {
    windows: Arc<Mutex<HashMap<String, (Instant, u32)>>>,
}

impl RateLimiter {
    /// Count a call for `key` and say whether it is within `limit` calls
    /// per `window`.
    pub fn allow(&self, key: &str, limit: u32, window: Duration) -> bool {
        self.allow_at(key, limit, window, Instant::now())
    }

    fn allow_at(&self, key: &str, limit: u32, window: Duration, now: Instant) -> bool {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let (start, count) = windows.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(*start) >= window {
            (*start, *count) = (now, 0);
        }
        *count += 1;
        *count <= limit
    }

    /// Forget `key`, for a webhook that was deleted.
    pub fn forget(&self, key: &str) {
        self.windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_key_and_window() {
        let r = RateLimiter::default();
        let t = Instant::now();
        let w = Duration::from_secs(60);
        assert!(r.allow_at("a", 2, w, t));
        assert!(r.allow_at("a", 2, w, t));
        assert!(!r.allow_at("a", 2, w, t));
        assert!(r.allow_at("b", 2, w, t), "keys are separate");
        assert!(r.allow_at("a", 2, w, t + w), "a new window starts over");
    }
}
