//! Round-robin API key pool with per-key cooldowns.

use parking_lot::Mutex;
use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyVerdict {
    /// 429 — back off this key for a while.
    RateLimited,
    /// 401 / 403 — quota exhausted or key rejected, park it for longer.
    QuotaOrAuth,
    /// 5xx — provider-side hiccup, short cooldown.
    ServerError,
    /// Timeouts and connection resets.
    Transient,
    /// 4xx that no rotation can fix (bad request, unknown model).
    Fatal,
}

impl KeyVerdict {
    fn cooldown(self) -> Option<Duration> {
        match self {
            KeyVerdict::RateLimited => Some(Duration::from_secs(60)),
            KeyVerdict::QuotaOrAuth => Some(Duration::from_secs(900)),
            KeyVerdict::ServerError => Some(Duration::from_secs(15)),
            KeyVerdict::Transient => Some(Duration::from_secs(5)),
            KeyVerdict::Fatal => None,
        }
    }
}

#[derive(Debug, Clone)]
struct KeyState {
    key: String,
    cooldown_until: Option<Instant>,
    failures: u32,
    successes: u32,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeyStatus {
    pub index: usize,
    pub masked: String,
    pub cooling_seconds: u64,
    pub failures: u32,
    pub successes: u32,
    pub last_error: Option<String>,
}

pub struct KeyLease {
    pub index: usize,
    pub key: String,
}

/// Thread-safe pool. Cheap to clone via `Arc`.
pub struct KeyPool {
    inner: Mutex<Inner>,
}

struct Inner {
    keys: Vec<KeyState>,
    cursor: usize,
}

impl KeyPool {
    pub fn new(keys: Vec<String>) -> Self {
        KeyPool {
            inner: Mutex::new(Inner {
                keys: keys
                    .into_iter()
                    .filter(|k| !k.trim().is_empty())
                    .map(|key| KeyState {
                        key,
                        cooldown_until: None,
                        failures: 0,
                        successes: 0,
                        last_error: None,
                    })
                    .collect(),
                cursor: 0,
            }),
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Round-robin over keys that are not cooling down.
    pub fn acquire(&self) -> Option<KeyLease> {
        let mut inner = self.inner.lock();
        let total = inner.keys.len();
        if total == 0 {
            return None;
        }
        let now = Instant::now();
        for offset in 0..total {
            let idx = (inner.cursor + offset) % total;
            let usable = match inner.keys[idx].cooldown_until {
                Some(until) => until <= now,
                None => true,
            };
            if usable {
                inner.keys[idx].cooldown_until = None;
                inner.cursor = (idx + 1) % total;
                return Some(KeyLease {
                    index: idx,
                    key: inner.keys[idx].key.clone(),
                });
            }
        }
        None
    }

    pub fn shortest_cooldown(&self) -> Option<Duration> {
        let inner = self.inner.lock();
        let now = Instant::now();
        inner
            .keys
            .iter()
            .filter_map(|k| k.cooldown_until)
            .map(|until| until.saturating_duration_since(now))
            .min()
    }

    pub fn report_success(&self, index: usize) {
        let mut inner = self.inner.lock();
        if let Some(state) = inner.keys.get_mut(index) {
            state.successes += 1;
            state.failures = 0;
            state.cooldown_until = None;
            state.last_error = None;
        }
    }

    pub fn report_failure(&self, index: usize, verdict: KeyVerdict) {
        let mut inner = self.inner.lock();
        if let Some(state) = inner.keys.get_mut(index) {
            state.failures += 1;
            state.last_error = Some(format!("{verdict:?}"));
            if let Some(base) = verdict.cooldown() {
                // Repeated failures on the same key stretch the cooldown.
                let factor = state.failures.min(4);
                state.cooldown_until = Some(Instant::now() + base * factor);
            }
        }
    }

    pub fn status(&self) -> Vec<KeyStatus> {
        let inner = self.inner.lock();
        let now = Instant::now();
        inner
            .keys
            .iter()
            .enumerate()
            .map(|(index, k)| KeyStatus {
                index,
                masked: crate::config::mask_key(&k.key),
                cooling_seconds: k
                    .cooldown_until
                    .map(|u| u.saturating_duration_since(now).as_secs())
                    .unwrap_or(0),
                failures: k.failures,
                successes: k.successes,
                last_error: k.last_error.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_round_robin() {
        let pool = KeyPool::new(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(pool.acquire().unwrap().key, "a");
        assert_eq!(pool.acquire().unwrap().key, "b");
        assert_eq!(pool.acquire().unwrap().key, "c");
        assert_eq!(pool.acquire().unwrap().key, "a");
    }

    #[test]
    fn skips_cooling_keys() {
        let pool = KeyPool::new(vec!["a".into(), "b".into()]);
        let lease = pool.acquire().unwrap();
        pool.report_failure(lease.index, KeyVerdict::RateLimited);
        let next = pool.acquire().unwrap();
        assert_eq!(next.key, "b");
        let again = pool.acquire().unwrap();
        assert_eq!(again.key, "b", "cooling key must stay parked");
    }

    #[test]
    fn empty_pool_yields_nothing() {
        let pool = KeyPool::new(vec![" ".into()]);
        assert!(pool.is_empty());
        assert!(pool.acquire().is_none());
    }
}
