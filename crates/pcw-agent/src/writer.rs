//! The writer thread (plan Task 12): batches write ops into one
//! `Store::apply_batch` transaction per flush — every 1 s or at 500 queued
//! ops, whichever first — with the two disk guards of spec §4.6.
//!
//! Guards:
//! - Every `disk_check_every` (60 s in the agent) `free_bytes` is polled;
//!   while the result is below `min_free_bytes` only UDP `Packet` ops are
//!   dropped (counted in `udp_skipped_low_disk`); TCP Open/Close, DNS,
//!   session and title ops still attempt a write. `None` (failed probe) is
//!   treated as disk NOT low — unknown must not drop anything.
//! - `StorageError::DiskFull` ⇒ the whole batch was rolled back by the store,
//!   so its op count is added to `rows_dropped_disk_full` and the writer
//!   continues with later batches.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pcw_foreground::OpenSession;
use pcw_storage::{SharedStore, StorageError, Store, WriteOp};
use pcw_types::{Counters, EventKind, Protocol};

/// `meta` counter names, used by exact string (review flag F6).
const UDP_SKIPPED_LOW_DISK: &str = "udp_skipped_low_disk";
const ROWS_DROPPED_DISK_FULL: &str = "rows_dropped_disk_full";

/// Flush cadence: one transaction per second.
const FLUSH_PERIOD: Duration = Duration::from_millis(1000);
/// …or one flush at this many queued ops, whichever comes first.
const FLUSH_OPS: usize = 500;

/// What callers feed the writer (Task 13's wiring contract).
pub enum WriterMsg {
    /// One write op for the next batch.
    Op(WriteOp),
    /// Tracker heartbeat every 60 s: persists (or clears) the open session.
    OpenSession {
        session: Option<OpenSession>,
        now_ms: u64,
    },
    /// Flush what is queued (spec §4: no data past shutdown, no duplicates)
    /// and return. Dropping the channel sender also ends the loop.
    Shutdown,
}

/// Budget + probe for the low-disk guard. `free_bytes` is injectable so
/// tests use fakes instead of real volumes.
pub struct WriterConfig {
    /// Below this many free bytes, UDP `Packet` ops start being dropped.
    pub min_free_bytes: u64,
    /// Free bytes of the data volume, `None` when the probe fails.
    pub free_bytes: Box<dyn Fn() -> Option<u64> + Send>,
    /// Check at most this often (the agent passes 60 s).
    pub disk_check_every: Duration,
}

/// Runs until `Shutdown` (or the channel closes), flushing what is queued so
/// the caller can `join()` without losing data. Never panics on a poisoned
/// store: a panic elsewhere must not take the writer with it.
pub fn run_writer(
    rx: Receiver<WriterMsg>,
    db: SharedStore,
    cfg: WriterConfig,
    counters: Arc<Counters>,
) {
    let mut queue: Vec<WriteOp> = Vec::new();
    let mut low_disk = false;
    // First check happens at start-up, then at most every config interval.
    let mut next_disk_check = Instant::now();
    let mut next_flush = Instant::now() + FLUSH_PERIOD;

    loop {
        if queue.len() >= FLUSH_OPS || Instant::now() >= next_flush {
            let now = now_ms();
            flush(&db, &counters, &mut queue, now);
            next_flush = Instant::now() + FLUSH_PERIOD;
        }
        if Instant::now() >= next_disk_check {
            low_disk = match (cfg.free_bytes)() {
                Some(free) => free < cfg.min_free_bytes,
                // Probe failed: unknown is not low (spec §4.6).
                None => false,
            };
            next_disk_check = Instant::now() + cfg.disk_check_every;
        }

        // Waiting up to the next flush deadline even with an idle channel is
        // what keeps a 1-second cadence; once the queue is full, the flush
        // already happened at the top of the loop, so we never spin.
        let wait = next_flush.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(WriterMsg::Op(op)) => {
                if low_disk && is_udp_packet(&op) {
                    counters.add(UDP_SKIPPED_LOW_DISK, 1);
                } else {
                    queue.push(op);
                }
            }
            Ok(WriterMsg::OpenSession { session, now_ms }) => {
                let mut store = lock(&db);
                if let Err(e) =
                    crate::state::save_open_session(&mut store, session.as_ref(), now_ms)
                {
                    log::error!("saving meta.open_session failed: {e}");
                }
            }
            Ok(WriterMsg::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {}
            // All senders dropped without Shutdown: still flush, then exit.
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    flush(&db, &counters, &mut queue, now_ms());
}

/// One transaction per flush. Every error discards the batch: `DiskFull`
/// additionally lands its op count in `rows_dropped_disk_full`; anything else
/// is logged (the store itself rolled the batch back).
fn flush(db: &Mutex<Store>, counters: &Counters, queue: &mut Vec<WriteOp>, now_ms: u64) {
    let ops = std::mem::take(queue);
    if ops.is_empty() {
        return;
    }
    let mut store = lock(db);
    if let Err(e) = store.apply_batch(&ops, now_ms) {
        match e {
            StorageError::DiskFull => {
                counters.add(ROWS_DROPPED_DISK_FULL, ops.len() as u64);
            }
            other => {
                log::error!("apply_batch failed, {} ops discarded: {other}", ops.len());
            }
        }
    }
}

fn lock(db: &Mutex<Store>) -> std::sync::MutexGuard<'_, Store> {
    match db.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::error!("writer lock poisoned, recovering the store anyway");
            poisoned.into_inner()
        }
    }
}

/// Only datagrams are guarded (spec §4.1: the UDP producer emits `Packet`).
fn is_udp_packet(op: &WriteOp) -> bool {
    matches!(op, WriteOp::Connection(ev)
        if ev.protocol == Protocol::Udp && ev.kind == EventKind::Packet)
}

/// Wall-clock milliseconds, 0 only if the clock is before the epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
