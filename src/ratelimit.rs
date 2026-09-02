//! Failed-login throttling.
//!
//! Argon2id already makes guessing expensive — that is most of the defence, and
//! it is why this can stay simple. What it does not stop is an attacker who is
//! content to grind slowly, or one who ties up the connection pool with hashing
//! work. A counter costs nothing and closes both.
//!
//! In memory, deliberately. A restart clearing the counters is not a meaningful
//! bypass when every attempt still costs an Argon2 hash, and persisting them
//! would mean a write path — and a table — for data that is worthless five
//! minutes after it is recorded.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_FAILURES: usize = 10;
const WINDOW: Duration = Duration::from_secs(5 * 60);

/// Tracks recent failures per key. Keys are both the login being attempted and
/// the client address, recorded separately: throttling only by login lets one
/// attacker work through many accounts freely, and throttling only by address
/// lets a distributed attempt through. Either limit being hit is enough to
/// refuse.
#[derive(Default)]
pub struct Limiter {
    failures: Mutex<HashMap<String, Vec<Instant>>>,
}

impl Limiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if this key has too many recent failures.
    pub fn is_blocked(&self, key: &str) -> bool {
        let mut map = match self.failures.lock() {
            Ok(m) => m,
            // A poisoned mutex means a panic while holding it. Failing open
            // here would disable throttling exactly when something is already
            // wrong, so treat it as blocked.
            Err(_) => return true,
        };
        match map.get_mut(key) {
            Some(hits) => {
                prune(hits);
                hits.len() >= MAX_FAILURES
            }
            None => false,
        }
    }

    /// Record one failed attempt.
    pub fn record_failure(&self, key: &str) {
        let Ok(mut map) = self.failures.lock() else {
            return;
        };
        let hits = map.entry(key.to_string()).or_default();
        prune(hits);
        hits.push(Instant::now());

        // Bound the map. Without this, an attacker cycling logins would grow it
        // without limit -- a slow memory leak driven by unauthenticated input.
        if map.len() > 10_000 {
            map.retain(|_, hits| {
                prune(hits);
                !hits.is_empty()
            });
        }
    }

    /// Forget a key's failures. Called on success so a legitimate user who
    /// mistyped their password a few times is not left throttled.
    pub fn clear(&self, key: &str) {
        if let Ok(mut map) = self.failures.lock() {
            map.remove(key);
        }
    }
}

fn prune(hits: &mut Vec<Instant>) {
    let now = Instant::now();
    hits.retain(|t| now.duration_since(*t) < WINDOW);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_key_is_not_blocked() {
        let l = Limiter::new();
        assert!(!l.is_blocked("nobody"));
    }

    #[test]
    fn blocks_only_after_the_threshold() {
        let l = Limiter::new();
        for i in 0..MAX_FAILURES - 1 {
            l.record_failure("ken");
            assert!(!l.is_blocked("ken"), "blocked early at attempt {i}");
        }
        l.record_failure("ken");
        assert!(l.is_blocked("ken"));
    }

    #[test]
    fn success_clears_the_count() {
        // A legitimate user who mistypes several times then gets it right must
        // not stay locked out.
        let l = Limiter::new();
        for _ in 0..MAX_FAILURES {
            l.record_failure("ken");
        }
        assert!(l.is_blocked("ken"));
        l.clear("ken");
        assert!(!l.is_blocked("ken"));
    }

    #[test]
    fn keys_are_independent() {
        let l = Limiter::new();
        for _ in 0..MAX_FAILURES {
            l.record_failure("ken");
        }
        assert!(l.is_blocked("ken"));
        assert!(!l.is_blocked("jaqui"), "one login throttled another");
    }
}
