//! Failed-login throttling: a client that fails too often inside a window
//! is refused until its oldest failure ages out. Clients are keyed by
//! address (the proxy-reported one when the proxy is trusted), and the
//! table is capped so a flood of addresses cannot grow it without bound.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ms_store::lock;

const MAXIMUM_TRACKED_CLIENTS: usize = 1024;

#[derive(Debug)]
pub struct Throttle {
    limit: usize,
    window: Duration,
    failures: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl Throttle {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self { limit, window, failures: Mutex::new(HashMap::new()) }
    }

    /// Seconds until `client` may try again, or `None` when it may now.
    pub fn locked_out(&self, client: &str) -> Option<u64> {
        let now = Instant::now();
        let mut failures = lock(&self.failures);
        let entry = failures.get_mut(client)?;
        while entry.front().is_some_and(|t| now.duration_since(*t) > self.window) {
            entry.pop_front();
        }
        if entry.len() < self.limit {
            return None;
        }
        let oldest = *entry.front()?;
        Some((self.window.saturating_sub(now.duration_since(oldest))).as_secs().max(1))
    }

    pub fn record_failure(&self, client: &str) {
        let mut failures = lock(&self.failures);
        if failures.len() >= MAXIMUM_TRACKED_CLIENTS && !failures.contains_key(client) {
            let now = Instant::now();
            let window = self.window;
            failures.retain(|_, v| v.back().is_some_and(|t| now.duration_since(*t) <= window));
            if failures.len() >= MAXIMUM_TRACKED_CLIENTS {
                return;
            }
        }
        let entry = failures.entry(client.to_owned()).or_default();
        entry.push_back(Instant::now());
        while entry.len() > self.limit {
            entry.pop_front();
        }
    }

    pub fn clear(&self, client: &str) {
        lock(&self.failures).remove(client);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_the_limit() {
        let t = Throttle::new(3, Duration::from_mins(15));
        t.record_failure("a");
        t.record_failure("a");
        assert_eq!(t.locked_out("a"), None);
        t.record_failure("a");
        assert!(t.locked_out("a").is_some());
        assert_eq!(t.locked_out("b"), None);
        t.clear("a");
        assert_eq!(t.locked_out("a"), None);
    }
}
