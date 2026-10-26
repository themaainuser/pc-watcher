//! Read path: query DTOs, filters and the query methods on `Store`
//! (plan Task 5). Consumed by export (Task 7), the PTR worker (Task 8) and
//! the dashboard (Task 14).
//!
//! Every connection query bounds `ts_first` into `[from_ms, to_ms)` (the
//! `idx_connections_ts_first` index drives them) and pulls the hostname with
//! a `LEFT JOIN hostnames ON h.ip = c.remote_ip`. Optional attributes become
//! `(?n IS NULL OR column = ?n)` terms so the statement shape is fixed and
//! all parameters are always bound.

use serde::Serialize;

use pcw_types::{AppSession, Direction, Protocol, WindowTitle};
use rusqlite::params;

use crate::write::{SqlResultExt, StorageError, Store, readonly_error};

/// Time-range and attribute filter shared by the connection queries.
/// Every connection query is bounded by `ts_first in [from_ms, to_ms)`.
#[derive(Debug, Clone)]
pub struct ConnFilter {
    pub from_ms: u64,
    /// Upper bound, exclusive.
    pub to_ms: u64,
    pub app: Option<String>,
    pub protocol: Option<Protocol>,
    /// Matches `remote_port` OR `local_port`.
    pub port: Option<u16>,
    pub direction: Option<Direction>,
}

impl Default for ConnFilter {
    fn default() -> Self {
        Self {
            from_ms: 0,
            to_ms: u64::MAX,
            app: None,
            protocol: None,
            port: None,
            direction: None,
        }
    }
}

/// One row of `connections`, joined with its hostname when the IP is known.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConnectionRow {
    pub id: i64,
    pub ts_first_ms: u64,
    pub ts_last_ms: u64,
    pub pid: u32,
    pub process_name: String,
    pub process_path: String,
    pub remote_ip: String,
    pub remote_port: u16,
    pub local_port: u16,
    pub protocol: Protocol,
    pub direction: Direction,
    pub hostname: Option<String>,
}

/// Aggregate counts for the dashboard overview: total connections, the ten
/// most-talked-to apps, the ten most-used services (hostname when known,
/// else the IP) and per-local-hour connection counts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OverviewSummary {
    pub total_connections: u64,
    pub top_apps: Vec<(String, u64)>,
    pub top_services: Vec<(String, u64)>,
    pub hourly: Vec<(u8, u64)>,
}

/// One remote destination of a single app, grouped by (ip, port, protocol).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DestinationRow {
    pub remote_ip: String,
    pub remote_port: u16,
    pub protocol: Protocol,
    pub hostname: Option<String>,
    pub count: u64,
    pub ts_first_ms: u64,
    pub ts_last_ms: u64,
}

/// One session count and total foreground time (whole seconds) of one app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct UsageSummary {
    pub session_count: u64,
    pub total_secs: u64,
}

/// Every schema column of `app_sessions` as one row (like [`ConnectionRow`]
/// for `connections`): the export paths ship whole rows, including `id`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: i64,
    pub ts_start_ms: u64,
    pub ts_end_ms: u64,
    pub process_name: String,
    pub window_title: String,
}

/// Every schema column of `window_titles` as one row (like [`ConnectionRow`]
/// for `connections`): the export paths ship whole rows, including `id`.
#[derive(Debug, Clone, PartialEq)]
pub struct TitleRow {
    pub id: i64,
    pub ts_ms: u64,
    pub process_name: String,
    pub title: String,
}

/// Filtered-connection predicate shared by all connection queries; binds six
/// parameters in this order: from, to, app, protocol, port, direction.
const CONN_WHERE: &str = "
  WHERE c.ts_first >= ?1 AND c.ts_first < ?2
    AND (?3 IS NULL OR c.process_name = ?3)
    AND (?4 IS NULL OR c.protocol = ?4)
    AND (?5 IS NULL OR c.remote_port = ?5 OR c.local_port = ?5)
    AND (?6 IS NULL OR c.direction = ?6)";

/// Explicit column list (no `SELECT *`) plus hostname join, for every query
/// returning [`ConnectionRow`]s.
const CONN_ROW_SELECT: &str = "
SELECT c.id, c.ts_first, c.ts_last, c.pid, c.process_name, c.process_path,
       c.remote_ip, c.remote_port, c.local_port, c.protocol, c.direction, h.hostname
  FROM connections c
  LEFT JOIN hostnames h ON h.ip = c.remote_ip";

/// A failed PTR attempt is retried only a day after it was recorded.
const PTR_RETRY_AFTER_MS: i64 = 86_400_000;

/// Timestamps are stored and read back as `i64` ms; the caller's `u64`
/// bounds are clamped, keeping the default `to_ms = u64::MAX` an unbounded
/// upper edge.
fn ts_bound(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

/// A value in an enum column that `Protocol::parse`/`Direction::parse`
/// rejects is a corrupt database, not a default-able difference.
fn bad_enum(column: &str, value: &str) -> StorageError {
    // The column is TEXT; the failure reports that type.
    StorageError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("unrecognized {column} value {value:?}").into(),
    ))
}

fn read_protocol(value: &str) -> Result<Protocol, StorageError> {
    Protocol::parse(value).ok_or_else(|| bad_enum("protocol", value))
}

fn read_direction(value: &str) -> Result<Direction, StorageError> {
    Direction::parse(value).ok_or_else(|| bad_enum("direction", value))
}

/// Maps one row of [`CONN_ROW_SELECT`] (column order as listed there).
fn connection_row(row: &rusqlite::Row<'_>) -> Result<ConnectionRow, StorageError> {
    let protocol = read_protocol(&row.get::<_, String>(9).se()?)?;
    let direction = read_direction(&row.get::<_, String>(10).se()?)?;
    Ok(ConnectionRow {
        id: row.get(0).se()?,
        ts_first_ms: row.get::<_, i64>(1).se()? as u64,
        ts_last_ms: row.get::<_, i64>(2).se()? as u64,
        pid: row.get::<_, i64>(3).se()? as u32,
        process_name: row.get(4).se()?,
        process_path: row.get(5).se()?,
        remote_ip: row.get(6).se()?,
        remote_port: row.get::<_, i64>(7).se()? as u16,
        local_port: row.get::<_, i64>(8).se()? as u16,
        protocol,
        direction,
        hostname: row.get(11).se()?,
    })
}

impl Store {
    pub fn overview(&self, f: &ConnFilter) -> Result<OverviewSummary, StorageError> {
        let conn = self.conn();
        let total_sql = format!("SELECT COUNT(*) FROM connections c{CONN_WHERE}");
        let total_connections: i64 = conn
            .query_row(
                &total_sql,
                params![
                    ts_bound(f.from_ms),
                    ts_bound(f.to_ms),
                    f.app,
                    f.protocol.map(Protocol::as_str),
                    f.port,
                    f.direction.map(Direction::as_str),
                ],
                |r| r.get(0),
            )
            .se()?;

        let top_apps_sql = format!(
            "SELECT c.process_name, COUNT(*) AS n FROM connections c{CONN_WHERE}
             GROUP BY c.process_name ORDER BY n DESC, c.process_name ASC LIMIT 10"
        );
        let mut top_apps = Vec::new();
        {
            let mut stmt = conn.prepare(&top_apps_sql).se()?;
            let mut rows = stmt
                .query(params![
                    ts_bound(f.from_ms),
                    ts_bound(f.to_ms),
                    f.app,
                    f.protocol.map(Protocol::as_str),
                    f.port,
                    f.direction.map(Direction::as_str),
                ])
                .se()?;
            while let Some(row) = rows.next().se()? {
                top_apps.push((
                    row.get::<_, String>(0).se()?,
                    row.get::<_, i64>(1).se()? as u64,
                ));
            }
        }

        let top_services_sql = format!(
            "SELECT COALESCE(h.hostname, c.remote_ip) AS svc, COUNT(*) AS n
             FROM connections c
             LEFT JOIN hostnames h ON h.ip = c.remote_ip{CONN_WHERE}
             GROUP BY svc ORDER BY n DESC, svc ASC LIMIT 10"
        );
        let mut top_services = Vec::new();
        {
            let mut stmt = conn.prepare(&top_services_sql).se()?;
            let mut rows = stmt
                .query(params![
                    ts_bound(f.from_ms),
                    ts_bound(f.to_ms),
                    f.app,
                    f.protocol.map(Protocol::as_str),
                    f.port,
                    f.direction.map(Direction::as_str),
                ])
                .se()?;
            while let Some(row) = rows.next().se()? {
                top_services.push((
                    row.get::<_, String>(0).se()?,
                    row.get::<_, i64>(1).se()? as u64,
                ));
            }
        }

        // Local-hour buckets: SQLite does the timezone-aware day split, so
        // the dashboard never re-implements it.
        let hourly_sql = format!(
            "SELECT CAST(strftime('%H', c.ts_first / 1000, 'unixepoch', 'localtime')
                   AS INTEGER) AS hr, COUNT(*) AS n
             FROM connections c{CONN_WHERE}
             GROUP BY hr ORDER BY hr ASC"
        );
        let mut hourly = Vec::new();
        {
            let mut stmt = conn.prepare(&hourly_sql).se()?;
            let mut rows = stmt
                .query(params![
                    ts_bound(f.from_ms),
                    ts_bound(f.to_ms),
                    f.app,
                    f.protocol.map(Protocol::as_str),
                    f.port,
                    f.direction.map(Direction::as_str),
                ])
                .se()?;
            while let Some(row) = rows.next().se()? {
                hourly.push((
                    row.get::<_, i64>(0).se()? as u8,
                    row.get::<_, i64>(1).se()? as u64,
                ));
            }
        }

        Ok(OverviewSummary {
            total_connections: total_connections as u64,
            top_apps,
            top_services,
            hourly,
        })
    }

    /// Groups one app's connections by (remote ip, remote port, protocol).
    /// `f.app` is required; with `None` set the result is empty.
    pub fn app_destinations(&self, f: &ConnFilter) -> Result<Vec<DestinationRow>, StorageError> {
        if f.app.is_none() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT c.remote_ip, c.remote_port, c.protocol, h.hostname,
                    COUNT(*) AS n, MIN(c.ts_first) AS tf, MAX(c.ts_last) AS tl
             FROM connections c
             LEFT JOIN hostnames h ON h.ip = c.remote_ip{CONN_WHERE}
             GROUP BY c.remote_ip, c.remote_port, c.protocol
             ORDER BY n DESC, c.remote_ip ASC, c.remote_port ASC, c.protocol ASC"
        );
        let mut stmt = self.conn().prepare(&sql).se()?;
        let mut rows = stmt
            .query(params![
                ts_bound(f.from_ms),
                ts_bound(f.to_ms),
                f.app,
                f.protocol.map(Protocol::as_str),
                f.port,
                f.direction.map(Direction::as_str),
            ])
            .se()?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().se()? {
            out.push(DestinationRow {
                remote_ip: row.get(0).se()?,
                remote_port: row.get::<_, i64>(1).se()? as u16,
                protocol: read_protocol(&row.get::<_, String>(2).se()?)?,
                hostname: row.get(3).se()?,
                count: row.get::<_, i64>(4).se()? as u64,
                ts_first_ms: row.get::<_, i64>(5).se()? as u64,
                ts_last_ms: row.get::<_, i64>(6).se()? as u64,
            });
        }
        Ok(out)
    }

    /// Newest connections first (descending `ts_first`, then `id`), capped at
    /// `limit` rows.
    pub fn connections(
        &self,
        f: &ConnFilter,
        limit: u32,
    ) -> Result<Vec<ConnectionRow>, StorageError> {
        let sql =
            format!("{CONN_ROW_SELECT}{CONN_WHERE} ORDER BY c.ts_first DESC, c.id DESC LIMIT ?7");
        let mut stmt = self.conn().prepare(&sql).se()?;
        let mut rows = stmt
            .query(params![
                ts_bound(f.from_ms),
                ts_bound(f.to_ms),
                f.app,
                f.protocol.map(Protocol::as_str),
                f.port,
                f.direction.map(Direction::as_str),
                limit as i64,
            ])
            .se()?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().se()? {
            out.push(connection_row(row)?);
        }
        Ok(out)
    }

    /// Connections whose hostname equals `key` OR whose remote IP equals
    /// `key`, newest first, capped at `limit` rows.
    pub fn service_connections(
        &self,
        key: &str,
        f: &ConnFilter,
        limit: u32,
    ) -> Result<Vec<ConnectionRow>, StorageError> {
        let sql = format!(
            "{CONN_ROW_SELECT}{CONN_WHERE}
             AND (c.remote_ip = ?7 OR h.hostname = ?7)
             ORDER BY c.ts_first DESC, c.id DESC LIMIT ?8"
        );
        let mut stmt = self.conn().prepare(&sql).se()?;
        let mut rows = stmt
            .query(params![
                ts_bound(f.from_ms),
                ts_bound(f.to_ms),
                f.app,
                f.protocol.map(Protocol::as_str),
                f.port,
                f.direction.map(Direction::as_str),
                key,
                limit as i64,
            ])
            .se()?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().se()? {
            out.push(connection_row(row)?);
        }
        Ok(out)
    }

    /// App sessions in `[from_ms, to_ms)` by `ts_start`, oldest first.
    pub fn sessions(
        &self,
        from_ms: u64,
        to_ms: u64,
        app: Option<&str>,
    ) -> Result<Vec<AppSession>, StorageError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT ts_start, ts_end, process_name, window_title
                 FROM app_sessions
                 WHERE ts_start >= ?1 AND ts_start < ?2
                   AND (?3 IS NULL OR process_name = ?3)
                 ORDER BY ts_start ASC, id ASC",
            )
            .se()?;
        let mut rows = stmt
            .query(params![ts_bound(from_ms), ts_bound(to_ms), app])
            .se()?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().se()? {
            out.push(AppSession {
                ts_start_ms: row.get::<_, i64>(0).se()? as u64,
                ts_end_ms: row.get::<_, i64>(1).se()? as u64,
                process_name: row.get(2).se()?,
                window_title: row.get(3).se()?,
            });
        }
        Ok(out)
    }

    /// Window titles in `[from_ms, to_ms)` by `ts`, oldest first.
    pub fn titles(
        &self,
        from_ms: u64,
        to_ms: u64,
        app: Option<&str>,
    ) -> Result<Vec<WindowTitle>, StorageError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT ts, process_name, title
                 FROM window_titles
                 WHERE ts >= ?1 AND ts < ?2
                   AND (?3 IS NULL OR process_name = ?3)
                 ORDER BY ts ASC, id ASC",
            )
            .se()?;
        let mut rows = stmt
            .query(params![ts_bound(from_ms), ts_bound(to_ms), app])
            .se()?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().se()? {
            out.push(WindowTitle {
                ts_ms: row.get::<_, i64>(0).se()? as u64,
                process_name: row.get(1).se()?,
                title: row.get(2).se()?,
            });
        }
        Ok(out)
    }

    /// Every schema column of `app_sessions`, including `id`, in
    /// `[from_ms, to_ms)` by `ts_start`, oldest first — the whole-row read
    /// the export paths use ([`Store::sessions`] stays the dashboard DTO).
    pub fn session_rows(
        &self,
        from_ms: u64,
        to_ms: u64,
        app: Option<&str>,
    ) -> Result<Vec<SessionRow>, StorageError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT id, ts_start, ts_end, process_name, window_title
                 FROM app_sessions
                 WHERE ts_start >= ?1 AND ts_start < ?2
                   AND (?3 IS NULL OR process_name = ?3)
                 ORDER BY ts_start ASC, id ASC",
            )
            .se()?;
        let mut rows = stmt
            .query(params![ts_bound(from_ms), ts_bound(to_ms), app])
            .se()?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().se()? {
            out.push(SessionRow {
                id: row.get(0).se()?,
                ts_start_ms: row.get::<_, i64>(1).se()? as u64,
                ts_end_ms: row.get::<_, i64>(2).se()? as u64,
                process_name: row.get(3).se()?,
                window_title: row.get(4).se()?,
            });
        }
        Ok(out)
    }

    /// Every schema column of `window_titles`, including `id`, in
    /// `[from_ms, to_ms)` by `ts`, oldest first — like [`Store::session_rows`].
    pub fn title_rows(
        &self,
        from_ms: u64,
        to_ms: u64,
        app: Option<&str>,
    ) -> Result<Vec<TitleRow>, StorageError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT id, ts, process_name, title
                 FROM window_titles
                 WHERE ts >= ?1 AND ts < ?2
                   AND (?3 IS NULL OR process_name = ?3)
                 ORDER BY ts ASC, id ASC",
            )
            .se()?;
        let mut rows = stmt
            .query(params![ts_bound(from_ms), ts_bound(to_ms), app])
            .se()?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().se()? {
            out.push(TitleRow {
                id: row.get(0).se()?,
                ts_ms: row.get::<_, i64>(1).se()? as u64,
                process_name: row.get(2).se()?,
                title: row.get(3).se()?,
            });
        }
        Ok(out)
    }

    /// Session count and total foreground time of one app (sub-second parts
    /// of the sum are truncated).
    pub fn usage_summary(
        &self,
        app: &str,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<UsageSummary, StorageError> {
        let (count, total_ms): (i64, i64) = self
            .conn()
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(ts_end - ts_start), 0)
                 FROM app_sessions
                 WHERE process_name = ?1 AND ts_start >= ?2 AND ts_start < ?3",
                params![app, ts_bound(from_ms), ts_bound(to_ms)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .se()?;
        Ok(UsageSummary {
            session_count: count as u64,
            total_secs: (total_ms / 1000) as u64,
        })
    }

    /// IPs the hostname worker should try next: still-pending rows first,
    /// then rows whose last attempt failed (`source = 'none'`) at least 24 h
    /// ago, oldest `resolved_at` first. Resolved rows ('ptr'/'dns') are never
    /// returned. `limit` bounds the combined list.
    pub fn pending_hostnames(&self, limit: u32, now_ms: u64) -> Result<Vec<String>, StorageError> {
        // Two scans of `idx_hostnames_source`: pending (priority 0) and aged
        // failures (priority 1, oldest `resolved_at` first; within a priority
        // ties break by ip for determinism). LIMIT cuts the combined list.
        let sql = format!(
            "
SELECT ip, 0, 0 FROM hostnames WHERE source = 'pending'
UNION ALL
SELECT ip, 1, resolved_at FROM hostnames
 WHERE source = 'none' AND resolved_at <= ?2 - {PTR_RETRY_AFTER_MS}
ORDER BY 2, 3, 1
LIMIT ?1"
        );
        let mut stmt = self.conn().prepare(&sql).se()?;
        let mut rows = stmt.query(params![limit, ts_bound(now_ms)]).se()?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().se()? {
            out.push(row.get::<_, String>(0).se()?);
        }
        Ok(out)
    }

    /// Records a PTR lookup result. `Some(name)` marks the row `source='ptr'`,
    /// `None` records a failure as `source='none'` so it is retried only after
    /// 24 h. Rows named by DNS (`source='dns'`) are never modified.
    pub fn set_ptr_result(
        &mut self,
        ip: &str,
        name: Option<&str>,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        if !self.writable() {
            return Err(readonly_error());
        }
        // `Some` ⇒ 'ptr', `None` ⇒ 'none'; the `source <> 'dns'` predicate
        // makes DNS-provided rows untouchable.
        self.conn()
            .execute(
                "UPDATE hostnames
                 SET hostname = ?2, source = ?3, resolved_at = ?4
                 WHERE ip = ?1 AND source <> 'dns'",
                params![
                    ip,
                    name,
                    if name.is_some() { "ptr" } else { "none" },
                    ts_bound(now_ms)
                ],
            )
            .se()?;
        Ok(())
    }

    /// Streams connections with `id > after_id` in ascending id order (export
    /// watermark paging; nothing is loaded fully into memory). Returns the
    /// highest id passed to `f`, or `after_id` when no row follows.
    pub fn for_each_connection_after(
        &self,
        after_id: i64,
        f: &mut dyn FnMut(&ConnectionRow) -> std::io::Result<()>,
    ) -> Result<i64, StorageError> {
        let sql = format!("{CONN_ROW_SELECT} WHERE c.id > ?1 ORDER BY c.id ASC");
        let mut stmt = self.conn().prepare(&sql).se()?;
        let mut rows = stmt.query(params![after_id]).se()?;
        let mut last = after_id;
        while let Some(row) = rows.next().se()? {
            let row = connection_row(row)?;
            last = row.id;
            f(&row)?;
        }
        Ok(last)
    }

    /// Streams app sessions like [`Store::for_each_connection_after`].
    pub fn for_each_session_after(
        &self,
        after_id: i64,
        f: &mut dyn FnMut(i64, &AppSession) -> std::io::Result<()>,
    ) -> Result<i64, StorageError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT id, ts_start, ts_end, process_name, window_title
                 FROM app_sessions WHERE id > ?1 ORDER BY id ASC",
            )
            .se()?;
        let mut rows = stmt.query(params![after_id]).se()?;
        let mut last = after_id;
        while let Some(row) = rows.next().se()? {
            let session = AppSession {
                ts_start_ms: row.get::<_, i64>(1).se()? as u64,
                ts_end_ms: row.get::<_, i64>(2).se()? as u64,
                process_name: row.get(3).se()?,
                window_title: row.get(4).se()?,
            };
            last = row.get::<_, i64>(0).se()?;
            f(last, &session)?;
        }
        Ok(last)
    }

    /// Streams window titles like [`Store::for_each_connection_after`].
    pub fn for_each_title_after(
        &self,
        after_id: i64,
        f: &mut dyn FnMut(i64, &WindowTitle) -> std::io::Result<()>,
    ) -> Result<i64, StorageError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT id, ts, process_name, title
                 FROM window_titles WHERE id > ?1 ORDER BY id ASC",
            )
            .se()?;
        let mut rows = stmt.query(params![after_id]).se()?;
        let mut last = after_id;
        while let Some(row) = rows.next().se()? {
            let title = WindowTitle {
                ts_ms: row.get::<_, i64>(1).se()? as u64,
                process_name: row.get(2).se()?,
                title: row.get(3).se()?,
            };
            last = row.get::<_, i64>(0).se()?;
            f(last, &title)?;
        }
        Ok(last)
    }
}
