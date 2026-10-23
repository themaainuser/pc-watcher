//! One-shot timer predicate (plan Task 12): `last_ms` absent ⇒ due now.

/// True when `now_ms` is at least `period_ms` past the last run. A `None`
/// last run (persisted marker absent, e.g. fresh install) is always due.
#[must_use]
pub fn due(now_ms: u64, last_ms: Option<u64>, period_ms: u64) -> bool {
    match last_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= period_ms,
    }
}
