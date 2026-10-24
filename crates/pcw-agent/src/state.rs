//! Cross-crash state: the open foreground session and the six counters, both
//! persisted through `meta` keys.
//!
//! `meta.open_session` is the JSON
//! `{ts_start_ms, process_name, window_title, saved_at_ms}` (spec §4.3, the
//! saved-at timestamp doubles as the session end). The counter keys are the
//! exact [`pcw_types::COUNTER_KEYS`] strings (review flag F6) as decimal
//! strings; manifest/export reads the same keys.

use serde::{Deserialize, Serialize};

use pcw_etw::supervisor::Supervisor;
use pcw_foreground::OpenSession;
use pcw_storage::{StorageError, Store, WriteOp};
use pcw_types::{AppSession, COUNTER_KEYS, Counters};

const OPEN_SESSION_KEY: &str = "open_session";

/// What `meta.open_session` holds while a session is open. Distinct struct
/// (not serializing `OpenSession` directly) because the JSON also carries
/// `saved_at_ms`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SavedOpenSession {
    pub ts_start_ms: u64,
    pub process_name: String,
    pub window_title: String,
    pub saved_at_ms: u64,
}

/// Snapshots the open session at `now_ms`, or clears the key when `None`
/// (session closed / tracker finalized at clean shutdown). Called by the
/// writer thread on every `WriterMsg::OpenSession`, every 60 s.
pub fn save_open_session(
    store: &mut Store,
    s: Option<&OpenSession>,
    now_ms: u64,
) -> Result<(), StorageError> {
    match s {
        Some(session) => {
            let saved = SavedOpenSession {
                ts_start_ms: session.ts_start_ms,
                process_name: session.process_name.clone(),
                window_title: session.window_title.clone(),
                saved_at_ms: now_ms,
            };
            let json = json_to_string(&saved)?;
            store.set_meta(OPEN_SESSION_KEY, &json)
        }
        None => store.delete_meta(OPEN_SESSION_KEY),
    }
}

/// Recovers the session saved by a previous run: inserts the session row
/// [`ts_start_ms`, `saved_at_ms`] and clears the key, so the data only ever
/// exists once. Returns the inserted session as a closed session row (spec
/// §4.3 requires only the write — the caller (Task 13) merely logs it, the
/// tracker is not re-opened on it). Absent or corrupt key ⇒ `Ok(None)`; a
/// corrupt JSON blob is deleted rather than resurrected (fresh installs
/// start clean).
pub fn recover_open_session(store: &mut Store) -> Result<Option<AppSession>, StorageError> {
    let Some(raw) = store.get_meta(OPEN_SESSION_KEY)? else {
        return Ok(None);
    };
    let saved = match serde_json::from_str::<SavedOpenSession>(&raw) {
        Ok(saved) => saved,
        Err(e) => {
            log::error!("meta.open_session is not valid JSON ({e}); dropping it");
            store.delete_meta(OPEN_SESSION_KEY)?;
            return Ok(None);
        }
    };
    let session = AppSession {
        ts_start_ms: saved.ts_start_ms,
        ts_end_ms: saved.saved_at_ms,
        process_name: saved.process_name,
        window_title: saved.window_title,
    };
    store.apply_batch(&[WriteOp::Session(session.clone())], saved.saved_at_ms)?;
    store.delete_meta(OPEN_SESSION_KEY)?;
    Ok(Some(session))
}

/// Writes all six counters as decimal strings in `COUNTER_KEYS` order.
/// Called periodically by the timer thread and at clean shutdown.
pub fn persist_counters(store: &mut Store, c: &Counters) -> Result<(), StorageError> {
    for (key, value) in c.snapshot() {
        store.set_meta(key, &value.to_string())?;
    }
    Ok(())
}

/// Loads the six counters. Absent or non-numeric keys leave the current value
/// (typically 0): a fresh install must not invent numbers (review flag F4).
pub fn load_counters(store: &Store, c: &Counters) -> Result<(), StorageError> {
    for key in COUNTER_KEYS {
        if let Some(raw) = store.get_meta(key)? {
            if let Ok(value) = raw.parse::<u64>() {
                c.set(key, value);
            }
        }
    }
    Ok(())
}

/// Builds this run's ETW supervisor from the loaded lifetime counters. The
/// three supervisor keys (`watchdog_restarts`, `etw_events_lost`,
/// `etw_buffers_lost`) are lifetime-cumulative, and the first timer tick
/// publishes the supervisor's values over them — a fresh supervisor would
/// reset them to 0 on that tick (review finding: lifetime supervisor
/// counters). Seeding gives the supervisor itself the cumulative semantics:
/// later `on_stats`/`on_exit` results add on top of the loaded totals, only
/// the per-session baselines reset.
pub fn resume_supervisor(counters: &Counters) -> Supervisor {
    Supervisor::with_totals(
        counters.get("watchdog_restarts"),
        counters.get("etw_events_lost"),
        counters.get("etw_buffers_lost"),
    )
}

/// Bridges a serde error into [`StorageError`]: `to_string` on these structs
/// cannot fail, so the message text is only a debugging aid.
fn json_to_string<T: Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|e| {
        StorageError::Sqlite(rusqlite::Error::InvalidParameterName(format!(
            "serializing meta.open_session: {e}"
        )))
    })
}
