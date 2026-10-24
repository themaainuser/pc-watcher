//! Reverse-DNS fallback worker (plan Task 8).
//!
//! IPs that connections saw but no DNS answer ever named are resolved
//! lazily here: `pending_hostnames` hands out the work list, `SystemResolver`
//! asks the OS (getaddrinfo via getnameinfo, `NI_NAMEREQD`) and `set_ptr_result`
//! records the outcome — `Some` ⇒ `source='ptr'`, `None` ⇒ `source='none'`
//! (a 24 h negative cache, because only failures older than a day are
//! retried). The store lock is taken only to read the work list and to write
//! each result, never while a DNS call is in flight.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dns_lookup::lookup_addr;
use log::error;
use thiserror::Error;

use pcw_storage::{SharedStore, StorageError};
/// Names an IP, or `None` when it has no PTR record.
pub trait NameResolver {
    fn lookup(&self, ip: &str) -> Option<String>;
}

/// The production resolver: `dns_lookup::lookup_addr`, the OS getnameinfo
/// call with `NI_NAMEREQD` — an IP with no PTR record is an error, so it does
/// not echo the IP back.
pub struct SystemResolver;

impl NameResolver for SystemResolver {
    fn lookup(&self, ip: &str) -> Option<String> {
        let addr: IpAddr = ip.parse().ok()?;
        lookup_addr(&addr).ok()
    }
}

/// Storage failures are the only way the worker can fail.
#[derive(Debug, Error)]
pub enum ResolveError {
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Locks the store, returning the guard even from a poisoned mutex: a panic
/// in another thread while writing must not wedge the worker.
fn lock(store: &SharedStore) -> MutexGuard<'_, pcw_storage::Store> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

/// Tries every IP `pending_hostnames` yields (up to `limit`) and stores each
/// result. Returns how many IPs were named. The lock is held only to read
/// the work list and to write each result per IP — lookups run unlocked, so
/// collectors and readers never wait on a DNS call.
pub fn resolve_pending(
    db: &SharedStore,
    resolver: &impl NameResolver,
    limit: u32,
    now_ms: u64,
) -> Result<usize, ResolveError> {
    let ips = {
        let store = lock(db);
        store.pending_hostnames(limit, now_ms)?
    };
    let mut named = 0;
    for ip in ips {
        let name = resolver.lookup(&ip);
        {
            let mut store = lock(db);
            store.set_ptr_result(&ip, name.as_deref(), now_ms)?;
        }
        if name.is_some() {
            named += 1;
        }
    }
    Ok(named)
}

/// One resolve round every `interval` until `stop` is set. Task 12 threads
/// this with `interval = 30 s`, `limit = 50`.
pub fn run_worker(db: SharedStore, stop: Arc<AtomicBool>, interval: Duration, limit: u32) {
    // Sleep in slices so stop is honored promptly even with a long interval.
    let slice = interval.min(Duration::from_millis(200));
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Storage errors (e.g. disk full in Task 10's mode) are not
        // crash-worthy for a background worker; the next round retries —
        // but the failure is logged, not swallowed silently.
        if let Err(e) = resolve_pending(&db, &SystemResolver, limit, now_ms()) {
            error!("resolve round failed: {e}");
        }
        let mut waited = Duration::ZERO;
        while waited < interval && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(slice);
            waited += slice;
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
