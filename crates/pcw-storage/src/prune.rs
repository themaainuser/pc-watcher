//! Retention pruning: watermark-guarded, chunked deletes (plan Task 6).
//!
//! A row is eligible only when its id is at or below the per-table export
//! watermark (`meta.export_wm_*`) — anything written after the last export
//! is never deleted (Review Focus #1) — **and** its timestamp is older than
//! the retention cutoff, `now_ms - retention_days * 86_400_000`. The
//! `hostnames` cache is never pruned. Each step runs in one transaction,
//! deletes at most `max_rows` rows in total from a shared budget across the
//! three event tables, and returns the number deleted; callers loop until it
//! returns 0 (Task 12 uses `max_rows = 50_000`).

use rusqlite::params;

use crate::write::{SqlResultExt, StorageError, Store, readonly_error};

const WM_CONNECTIONS: &str = "export_wm_connections";
const WM_SESSIONS: &str = "export_wm_sessions";
const WM_TITLES: &str = "export_wm_titles";
const DAY_MS: u64 = 86_400_000;

impl Store {
    /// Deletes up to `max_rows` expired, already-exported rows in one
    /// transaction and returns how many rows were removed across
    /// `connections` (by `ts_first`), `app_sessions` (by `ts_start`) and
    /// `window_titles` (by `ts`).
    ///
    /// A missing or unparseable watermark is 0, matching no row id (ids
    /// start at 1), so a never-exported table loses nothing. A read-only
    /// store cannot prune at all.
    pub fn prune_step(
        &mut self,
        retention_days: u32,
        now_ms: u64,
        max_rows: u32,
    ) -> Result<u64, StorageError> {
        if !self.writable() {
            return Err(readonly_error());
        }
        let wm_connections = watermark(self.get_meta(WM_CONNECTIONS)?);
        let wm_sessions = watermark(self.get_meta(WM_SESSIONS)?);
        let wm_titles = watermark(self.get_meta(WM_TITLES)?);
        let cutoff = now_ms.saturating_sub(u64::from(retention_days) * DAY_MS) as i64;

        let tx = self.conn_mut().transaction().se()?;
        let mut budget = i64::from(max_rows);
        let mut deleted = 0u64;
        deleted += prune_table(
            &tx,
            "connections",
            "ts_first",
            wm_connections,
            cutoff,
            &mut budget,
        )?;
        deleted += prune_table(
            &tx,
            "app_sessions",
            "ts_start",
            wm_sessions,
            cutoff,
            &mut budget,
        )?;
        deleted += prune_table(&tx, "window_titles", "ts", wm_titles, cutoff, &mut budget)?;
        tx.commit().se()?;
        Ok(deleted)
    }
}

/// Parses a watermark value; absent or not a valid i64 means 0, which can
/// never match a row id, so nothing is deleted from that table.
fn watermark(raw: Option<String>) -> i64 {
    raw.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0)
}

/// Deletes eligible rows of one event table within the shared `budget`.
/// The table and column names are compile-time constants (no SQL injection
/// surface); the watermark, cutoff and remaining budget are bound
/// parameters.
fn prune_table(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    ts_col: &str,
    watermark: i64,
    cutoff: i64,
    budget: &mut i64,
) -> Result<u64, StorageError> {
    if *budget == 0 {
        return Ok(0);
    }
    let sql = format!(
        "DELETE FROM {table} \
         WHERE id IN (SELECT id FROM {table} WHERE id <= ?1 AND {ts_col} < ?2 LIMIT ?3)"
    );
    let n = tx
        .prepare(&sql)
        .se()?
        .execute(params![watermark, cutoff, *budget])
        .se()? as i64;
    *budget -= n;
    Ok(n as u64)
}
