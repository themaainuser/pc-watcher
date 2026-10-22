//! Write path: `Store`, `WriteOp::apply_batch`, and meta helpers.
//!
//! `apply_batch` runs one transaction per batch and keeps three in-memory
//! structures: the open TCP connection map (key → row id + open ts), the
//! seen-IP set and the DNS name map (ip → name, capped). All three start
//! empty on `open()`.

use std::collections::{HashMap, HashSet, VecDeque, hash_map::Entry};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use thiserror::Error;

use pcw_types::{AppSession, ConnectionEvent, DnsAnswer, EventKind, Protocol, WindowTitle};

use crate::schema::SCHEMA;

/// Open TCP connections less than 24 h old are dropped (lost close events).
const OPEN_MAP_LIFETIME_MS: u64 = 24 * 3_600_000;
/// Open-map sweep runs at most once an hour.
const SWEEP_INTERVAL_MS: u64 = 3_600_000;
/// Seen-IP set is cleared when it reaches this size.
const SEEN_IP_CAP: usize = 100_000;
/// DNS map cap: when reached, the first-inserted (oldest by insertion) name
/// is evicted on the next new entry; refreshes keep their position.
const DNS_MAP_CAP: usize = 50_000;
const BUSY_TIMEOUT_MS: u64 = 5000;
const SCHEMA_VERSION: &str = "1";

pub type SharedStore = Arc<Mutex<Store>>;

#[derive(Debug, Error)]
pub enum StorageError {
    /// Any SQLite failure other than a full disk.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database could not grow (SQLITE_FULL); the batch was rolled back
    /// and must be discarded (spec §4.6 low-disk guard).
    #[error("disk full: batch rolled back and discarded")]
    DiskFull,
    /// A `Write`/`Flush` callback failed while streaming rows out (reader).
    #[error(transparent)]
    Callback(#[from] std::io::Error),
}

/// One record per write batch item, fed from the collectors.
#[derive(Debug, Clone)]
pub enum WriteOp {
    Connection(ConnectionEvent),
    Session(AppSession),
    Title(WindowTitle),
    Dns(DnsAnswer),
}

const SQL_SET_SCHEMA_VERSION: &str =
    "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)";
const SQL_INSERT_CONNECTION: &str = "
INSERT INTO connections(ts_first, ts_last, pid, process_name, process_path,
                        remote_ip, remote_port, local_port, protocol, direction)
VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";
const SQL_UPDATE_TS_LAST: &str = "
UPDATE connections SET ts_last = ?1 WHERE id = ?2";
const SQL_INSERT_SESSION: &str = "
INSERT INTO app_sessions(ts_start, ts_end, process_name, window_title)
VALUES (?1, ?2, ?3, ?4)";
const SQL_INSERT_TITLE: &str = "
INSERT INTO window_titles(ts, process_name, title)
VALUES (?1, ?2, ?3)";
const SQL_INSERT_HOSTNAME_NAMED: &str = "
INSERT OR IGNORE INTO hostnames(ip, hostname, source, resolved_at)
VALUES (?1, ?2, 'dns', ?3)";
const SQL_INSERT_HOSTNAME_PENDING: &str = "
INSERT OR IGNORE INTO hostnames(ip, hostname, source, resolved_at)
VALUES (?1, NULL, 'pending', 0)";
const SQL_SET_DNS_HOSTNAME: &str = "
UPDATE hostnames SET hostname = ?1, source = 'dns', resolved_at = ?2 WHERE ip = ?3";
const SQL_SET_META: &str = "
INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)";
const SQL_GET_META: &str = "
SELECT value FROM meta WHERE key = ?1";
const SQL_DELETE_META: &str = "
DELETE FROM meta WHERE key = ?1";

/// Open TCP connections are keyed by (local port, remote IP, remote port);
/// the PID is deliberately not part of the key, so a close reported in
/// another process context still matches (spec §4.4).
type TcpKey = (u16, String, u16);

/// The writer's in-memory state, all of it starting empty on `open()`.
#[derive(Debug, Clone, Default)]
struct State {
    /// Key → (connection row id, ts_ms at open).
    open: HashMap<TcpKey, (i64, u64)>,
    /// `now_ms` of the last open-map sweep (0 = sweep on the first batch).
    last_sweep_ms: u64,
    /// Remote IPs already recorded in `hostnames` (bounded, cleared at cap).
    seen_ips: HashSet<String>,
    /// ip → most recent DNS name, insertion-ordered, capped at 50,000.
    dns_names: HashMap<String, String>,
    dns_order: VecDeque<String>,
}

impl State {
    /// Marks a remote IP as seen; returns true when it is new, i.e. when the
    /// caller must attempt the `hostnames` insert. The set is cleared in one
    /// step once it reaches `SEEN_IP_CAP`; later re-attempts are
    /// `INSERT OR IGNORE`, so nothing is duplicated in the database.
    fn note_ip(&mut self, ip: &str) -> bool {
        if !self.seen_ips.insert(ip.to_string()) {
            return false;
        }
        if self.seen_ips.len() >= SEEN_IP_CAP {
            self.seen_ips.clear();
        }
        true
    }

    fn dns_name(&self, ip: &str) -> Option<&str> {
        self.dns_names.get(ip).map(String::as_str)
    }

    /// Puts or refreshes a name. Refreshes keep the original position, so
    /// "oldest" is by first insertion. Eviction pops from the front of
    /// `dns_order`; a refreshed key only moves its name, not its position.
    fn dns_insert(&mut self, ip: String, name: String) {
        match self.dns_names.entry(ip) {
            Entry::Occupied(mut slot) => {
                // A refreshed name keeps its original position.
                slot.insert(name);
            }
            Entry::Vacant(slot) => {
                let ip = slot.key().clone();
                drop(slot); // release the map borrow; eviction mutates both maps
                if self.dns_names.len() >= DNS_MAP_CAP {
                    if let Some(oldest) = self.dns_order.pop_front() {
                        self.dns_names.remove(&oldest);
                    }
                }
                self.dns_order.push_back(ip.clone());
                self.dns_names.insert(ip, name);
            }
        }
    }

    /// Drops open-map keys with no close for 24 h; checked at most once an
    /// hour (`last_sweep_ms` == 0 makes the first batch sweep).
    fn sweep_open_connections(&mut self, now_ms: u64) {
        if now_ms.saturating_sub(self.last_sweep_ms) < SWEEP_INTERVAL_MS {
            return;
        }
        self.last_sweep_ms = now_ms;
        let cutoff = now_ms.saturating_sub(OPEN_MAP_LIFETIME_MS);
        self.open.retain(|_, &mut (_, ts)| ts >= cutoff);
    }
}

pub struct Store {
    conn: Connection,
    writable: bool,
    state: State,
}

impl Store {
    /// Opens (creating parent dirs) a read/write database, applies the schema
    /// idempotently and pins `meta.schema_version = "1"`. WAL mode plus a 5 s
    /// busy timeout let the dashboard read while the agent writes.
    pub fn open(path: &Path) -> Result<Store, StorageError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                        Some(format!(
                            "cannot create database directory {}: {e}",
                            parent.display()
                        )),
                    ))
                })?;
            }
        }
        let conn = Connection::open(path)?;
        Self::initialize(conn, true)
    }

    /// Opens an existing database read-only through a `file:<path>?mode=ro`
    /// URI. Schema and pragmas are left untouched; any write returns `Err`.
    pub fn open_readonly(path: &Path) -> Result<Store, StorageError> {
        let uri = format!("file:{}?mode=ro", path.to_string_lossy().replace('\\', "/"));
        let conn = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        Self::initialize(conn, false)
    }

    /// Common connection setup. Both read/write and read-only stores get a 5 s
    /// busy timeout so readers queue behind the writer instead of failing with
    /// SQLITE_BUSY. A read-only store skips schema/pragma writes:
    /// `PRAGMA journal_mode=WAL` would be a write it cannot perform.
    fn initialize(conn: Connection, writable: bool) -> Result<Store, StorageError> {
        conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
            .se()?;
        let store = Store {
            conn,
            writable,
            state: State::default(),
        };
        if !writable {
            return Ok(store);
        }
        // journal_mode is a query pragma; it returns the resulting mode.
        let journal_mode: String = store
            .conn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .se()?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some(format!(
                    "could not enable WAL mode (journal_mode = {journal_mode})"
                )),
            )));
        }
        store.conn.execute_batch(SCHEMA)?;
        store
            .conn
            .execute(SQL_SET_SCHEMA_VERSION, params![SCHEMA_VERSION])?;
        Ok(store)
    }

    /// Applies one batch of write ops in a single transaction and returns the
    /// number of rows inserted or updated.
    ///
    /// On any error — notably SQLITE_FULL — the transaction is rolled back and
    /// the in-memory state is restored from a snapshot taken before the batch,
    /// so keys/names/ips added by the failed batch are discarded (spec §4.6:
    /// "that batch is dropped and counted, and the agent keeps running").
    pub fn apply_batch(&mut self, ops: &[WriteOp], now_ms: u64) -> Result<u64, StorageError> {
        if !self.writable {
            return Err(readonly_error());
        }
        let snapshot = self.state.clone();
        self.apply_batch_inner(ops, now_ms).inspect_err(|_| {
            self.state = snapshot;
        })
    }

    fn apply_batch_inner(&mut self, ops: &[WriteOp], now_ms: u64) -> Result<u64, StorageError> {
        let tx = self.conn.transaction().se()?;
        let mut rows = 0u64;
        self.state.sweep_open_connections(now_ms);
        for op in ops {
            match op {
                WriteOp::Connection(ev) => {
                    apply_connection(&tx, ev, &mut self.state, now_ms, &mut rows)?
                }
                WriteOp::Session(s) => {
                    let n = tx
                        .prepare_cached(SQL_INSERT_SESSION)
                        .se()?
                        .execute(params![
                            s.ts_start_ms as i64,
                            s.ts_end_ms as i64,
                            s.process_name,
                            s.window_title,
                        ])
                        .se()?;
                    rows += n as u64;
                }
                WriteOp::Title(t) => {
                    let n = tx
                        .prepare_cached(SQL_INSERT_TITLE)
                        .se()?
                        .execute(params![t.ts_ms as i64, t.process_name, t.title])
                        .se()?;
                    rows += n as u64;
                }
                WriteOp::Dns(answer) => {
                    for ip in &answer.ips {
                        self.state.dns_insert(ip.clone(), answer.name.clone());
                        // Only rows already recorded are touched; answers for
                        // never-connected IPs store nothing (§4.5).
                        let n = tx
                            .prepare_cached(SQL_SET_DNS_HOSTNAME)
                            .se()?
                            .execute(params![answer.name, now_ms as i64, ip])
                            .se()?;
                        rows += n as u64;
                    }
                }
            }
        }
        tx.commit().se()?;
        Ok(rows)
    }

    pub fn set_meta(&mut self, key: &str, val: &str) -> Result<(), StorageError> {
        if !self.writable {
            return Err(readonly_error());
        }
        self.conn
            .prepare_cached(SQL_SET_META)
            .se()?
            .execute(params![key, val])
            .se()?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>, StorageError> {
        let value = self
            .conn
            .query_row(SQL_GET_META, params![key], |row| row.get::<_, String>(0))
            .optional()?;
        Ok(value)
    }

    pub fn delete_meta(&mut self, key: &str) -> Result<(), StorageError> {
        if !self.writable {
            return Err(readonly_error());
        }
        self.conn
            .prepare_cached(SQL_DELETE_META)
            .se()?
            .execute(params![key])
            .se()?;
        Ok(())
    }

    /// Test-only escape hatch (`PRAGMA max_page_count`, Task 6 seeding).
    #[doc(hidden)]
    pub fn conn_for_tests(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// Writes one connection event and keeps the open-TCP map in step (§4.4).
fn apply_connection(
    tx: &rusqlite::Transaction<'_>,
    ev: &ConnectionEvent,
    state: &mut State,
    now_ms: u64,
    rows: &mut u64,
) -> Result<(), StorageError> {
    match ev.kind {
        EventKind::Open if ev.protocol == Protocol::Tcp => {
            let key = tcp_key(ev);
            if let Entry::Vacant(slot) = state.open.entry(key) {
                // Duplicate open (start-up snapshot plus live event): ignored.
                insert_connection(tx, ev)?;
                let id = tx.last_insert_rowid();
                slot.insert((id, ev.ts_ms));
                *rows += 1;
            }
        }
        EventKind::Close if ev.protocol == Protocol::Tcp => {
            let key = tcp_key(ev);
            if let Some((id, _)) = state.open.remove(&key) {
                let n = tx
                    .prepare_cached(SQL_UPDATE_TS_LAST)
                    .se()?
                    .execute(params![ev.ts_ms as i64, id])
                    .se()?;
                *rows += n as u64;
            } else {
                // Opened before collection and missed by the snapshot:
                // standalone row, direction taken from the event.
                insert_connection(tx, ev)?;
                *rows += 1;
            }
        }
        // Every UDP datagram is its own row.
        EventKind::Packet => {
            insert_connection(tx, ev)?;
            *rows += 1;
        }
        // Open/Close carries only a TCP meaning in this pipeline; the UDP
        // producer emits Packet (spec §4.1).
        EventKind::Open | EventKind::Close => {
            insert_connection(tx, ev)?;
            *rows += 1;
        }
    }
    // First time this IP is seen since the last clear: record it with its DNS
    // name if the in-memory map has one, else leave it for the PTR worker.
    if state.note_ip(&ev.remote_ip) {
        *rows += insert_hostname(tx, &ev.remote_ip, state.dns_name(&ev.remote_ip), now_ms)?;
    }
    Ok(())
}

fn tcp_key(ev: &ConnectionEvent) -> TcpKey {
    (ev.local_port, ev.remote_ip.clone(), ev.remote_port)
}

fn insert_connection(
    tx: &rusqlite::Transaction<'_>,
    ev: &ConnectionEvent,
) -> Result<(), StorageError> {
    tx.prepare_cached(SQL_INSERT_CONNECTION)
        .se()?
        .execute(params![
            ev.ts_ms as i64,
            ev.pid,
            ev.process_name,
            ev.process_path,
            ev.remote_ip,
            ev.remote_port,
            ev.local_port,
            ev.protocol.as_str(),
            ev.direction.as_str(),
        ])
        .se()?;
    Ok(())
}

fn insert_hostname(
    tx: &rusqlite::Transaction<'_>,
    ip: &str,
    name: Option<&str>,
    now_ms: u64,
) -> Result<u64, StorageError> {
    let n = match name {
        Some(name) => tx
            .prepare_cached(SQL_INSERT_HOSTNAME_NAMED)
            .se()?
            .execute(params![ip, name, now_ms as i64])
            .se()?,
        None => tx
            .prepare_cached(SQL_INSERT_HOSTNAME_PENDING)
            .se()?
            .execute(params![ip])
            .se()?,
    };
    Ok(n as u64)
}

/// Maps a `rusqlite::Error`, recognizing SQLITE_FULL as the disk-full condition.
fn storage_error(err: rusqlite::Error) -> StorageError {
    if err.sqlite_error_code() == Some(rusqlite::ffi::ErrorCode::DiskFull) {
        StorageError::DiskFull
    } else {
        StorageError::Sqlite(err)
    }
}

/// The same failure SQLite reports for a write attempt in `mode=ro`, produced
/// before a doomed transaction is even opened.
fn readonly_error() -> StorageError {
    StorageError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_READONLY),
        Some("attempt to write a readonly database".to_string()),
    ))
}

/// Every statement in the hot path goes through this so SQLITE_FULL is always
/// recognized as [`StorageError::DiskFull`], not only at `commit`.
trait SqlResultExt<T> {
    fn se(self) -> Result<T, StorageError>;
}

impl<T> SqlResultExt<T> for Result<T, rusqlite::Error> {
    fn se(self) -> Result<T, StorageError> {
        self.map_err(storage_error)
    }
}
