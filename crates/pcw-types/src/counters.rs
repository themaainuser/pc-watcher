use std::sync::atomic::{AtomicU64, Ordering};

pub const COUNTER_KEYS: [&str; 6] = [
    "etw_events_lost",
    "etw_buffers_lost",
    "watchdog_restarts",
    "udp_capped",
    "udp_skipped_low_disk",
    "rows_dropped_disk_full",
];

/// Six atomic counters, keyed by the exact `COUNTER_KEYS` array;
/// `snapshot` iterates in `COUNTER_KEYS` order.
#[derive(Debug, Default)]
pub struct Counters {
    counters: [AtomicU64; COUNTER_KEYS.len()], // COUNTER_KEYS[i] <-> counters[i]
}

impl Counters {
    fn slot(&self, key: &str) -> Option<&AtomicU64> {
        COUNTER_KEYS
            .iter()
            .position(|k| *k == key)
            .map(|i| &self.counters[i])
    }

    pub fn add(&self, key: &str, n: u64) {
        if let Some(slot) = self.slot(key) {
            slot.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub fn set(&self, key: &str, v: u64) {
        if let Some(slot) = self.slot(key) {
            slot.store(v, Ordering::Relaxed);
        }
    }

    /// Current value of one counter key (0 for an unknown key).
    pub fn get(&self, key: &str) -> u64 {
        self.slot(key)
            .map_or(0, |slot| slot.load(Ordering::Relaxed))
    }

    /// Returns all six counters as (key, value), in `COUNTER_KEYS` order.
    pub fn snapshot(&self) -> Vec<(&'static str, u64)> {
        COUNTER_KEYS
            .iter()
            .zip(self.counters.iter())
            .map(|(k, c)| (*k, c.load(Ordering::Relaxed)))
            .collect()
    }
}
