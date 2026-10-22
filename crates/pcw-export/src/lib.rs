//! PC-Watcher CSV/JSON export (plan Task 7, spec §7): snapshot folders with
//! `connections.csv`, `app-usage.csv`, `window-titles.csv`, `summary.json`
//! and `manifest.json`, streamed row-by-row out of SQLite in id order.
//!
//! Id watermarks (`export_wm_*` in `meta`) make each export incremental: only
//! rows written after the previous successful export ship again. The folder
//! is written under a `<name>.partial/` staging directory and renamed only
//! once complete; watermarks move strictly after a successful rename.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::PoisonError;

use serde::Serialize;
use thiserror::Error;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

use pcw_storage::{ConnFilter, ConnectionRow, SharedStore, StorageError, Store};
use pcw_types::{AppSession, COUNTER_KEYS, WindowTitle};

/// Result of one successful export: the renamed folder plus the number of
/// rows written to each CSV.
#[derive(Debug, Clone)]
pub struct ExportReport {
    pub folder: PathBuf,
    pub connection_rows: u64,
    pub session_rows: u64,
    pub title_rows: u64,
}

#[derive(Debug, Error)]
pub enum ExportError {
    /// `export_folder` does not exist and its parent does not either.
    #[error("export folder missing: {0}")]
    FolderMissing(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Storage(#[from] pcw_storage::StorageError),
    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

const CONN_HEADER: [&str; 10] = [
    "ts_first",
    "ts_last",
    "process_name",
    "process_path",
    "remote_ip",
    "remote_port",
    "local_port",
    "protocol",
    "direction",
    "hostname",
];
const USAGE_HEADER: [&str; 5] = [
    "ts_start",
    "ts_end",
    "duration_secs",
    "process_name",
    "window_title",
];
const TITLE_HEADER: [&str; 3] = ["ts", "process_name", "title"];

/// Bound on suffix search so a corrupt export root cannot loop forever.
const NAME_SUFFIX_LIMIT: u32 = 10_000;

/// All values the staging phase hands over to the commit phase.
struct Staged {
    connection_rows: u64,
    session_rows: u64,
    title_rows: u64,
    wm_connections: i64,
    wm_sessions: i64,
    wm_titles: i64,
}

#[derive(Serialize)]
struct RangeJson {
    from_ms: u64,
    to_ms: u64,
}

#[derive(Serialize)]
struct SummaryJson {
    total_connections: u64,
    top_apps: Vec<(String, u64)>,
    top_services: Vec<(String, u64)>,
    range: RangeJson,
}

#[derive(Serialize)]
struct ManifestJson {
    connection_rows: u64,
    session_rows: u64,
    title_rows: u64,
    from_ms: u64,
    to_ms: u64,
    generated_at_ms: u64,
    schema_version: String,
    tool_version: &'static str,
    /// The six `COUNTER_KEYS` from meta; a BTreeMap is a JSON object keyed
    /// deterministically regardless of insertion order.
    counters: std::collections::BTreeMap<String, u64>,
}

/// Runs one export cycle (spec §7): stages a dated snapshot folder under
/// `export_folder`, streams every not-yet-exported row into the three CSVs,
/// writes `summary.json`/`manifest.json`, renames the staged folder into
/// place and only then advances the id watermarks and `last_export` on the
/// writer's meta table.
///
/// `now_ms` is treated as the export instant and the exclusive `to_ms` bound
/// of the summarized range.
pub fn run_export(
    db_path: &Path,
    writer: &SharedStore,
    export_folder: &Path,
    now_ms: u64,
) -> Result<ExportReport, ExportError> {
    let base = ensure_base(export_folder)?;
    let name = pick_name(&base, &local_date(now_ms))?;
    let final_dir = base.join(&name);
    let partial_dir = base.join(format!("{name}.partial"));

    let staged = match stage_export(db_path, &partial_dir, now_ms) {
        Ok(staged) => staged,
        Err(err) => {
            let _ = fs::remove_dir_all(&partial_dir);
            return Err(err);
        }
    };

    let report = match fs::rename(&partial_dir, &final_dir) {
        Ok(()) => {
            let report = ExportReport {
                folder: final_dir,
                connection_rows: staged.connection_rows,
                session_rows: staged.session_rows,
                title_rows: staged.title_rows,
            };
            commit_watermarks(writer, now_ms, &staged)?;
            report
        }
        Err(err) => {
            let _ = fs::remove_dir_all(&partial_dir);
            return Err(ExportError::Io(err));
        }
    };

    Ok(report)
}

/// `export_folder` is left alone when it exists; otherwise it is created only
/// when its (non-empty) parent exists, else `FolderMissing` with the path.
fn ensure_base(export_folder: &Path) -> Result<PathBuf, ExportError> {
    if export_folder.is_dir() {
        return Ok(export_folder.to_path_buf());
    }
    let parent_exists = export_folder
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty() && parent.is_dir());
    if parent_exists {
        fs::create_dir(export_folder).map_err(ExportError::Io)?;
        Ok(export_folder.to_path_buf())
    } else {
        Err(ExportError::FolderMissing(export_folder.to_path_buf()))
    }
}

/// First free folder name: `YYYY-MM-DD`, then `-2`, `-3`, …
fn pick_name(base: &Path, date: &str) -> Result<String, ExportError> {
    let mut suffix = 1u32;
    let mut candidate = date.to_string();
    while base.join(&candidate).exists() {
        suffix += 1;
        if suffix > NAME_SUFFIX_LIMIT {
            return Err(ExportError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("no free export folder for {date}"),
            )));
        }
        candidate = format!("{date}-{suffix}");
    }
    Ok(candidate)
}

/// Streams everything new into the staged folder. Returns after all files are
/// flushed and closed; the caller renames.
fn stage_export(db_path: &Path, dir: &Path, now_ms: u64) -> Result<Staged, ExportError> {
    fs::create_dir(dir).map_err(ExportError::Io)?;
    let store = Store::open_readonly(db_path).map_err(ExportError::Storage)?;
    let offset = local_offset();

    let mut conns = new_csv(dir, "connections.csv", &CONN_HEADER)?;
    let mut usage = new_csv(dir, "app-usage.csv", &USAGE_HEADER)?;
    let mut titles = new_csv(dir, "window-titles.csv", &TITLE_HEADER)?;

    let mut min_ts: Option<u64> = None;
    let (mut n_conn, mut n_sess, mut n_title) = (0u64, 0u64, 0u64);

    let wm_connections = meta_i64(&store, "export_wm_connections")?;
    let wm_sessions = meta_i64(&store, "export_wm_sessions")?;
    let wm_titles = meta_i64(&store, "export_wm_titles")?;

    let wm_new_connections = store
        .for_each_connection_after(wm_connections, &mut |row: &ConnectionRow| {
            min_ts = Some(min_ts.map_or(row.ts_first_ms, |m| m.min(row.ts_first_ms)));
            n_conn += 1;
            let record = [
                rfc3339(row.ts_first_ms, offset),
                rfc3339(row.ts_last_ms, offset),
                row.process_name.clone(),
                row.process_path.clone(),
                row.remote_ip.clone(),
                row.remote_port.to_string(),
                row.local_port.to_string(),
                // Enums are CSV-wide lowercase ("tcp"/"out"), never the
                // serde/Debug names "Tcp"/"Out".
                row.protocol.as_str().to_string(),
                row.direction.as_str().to_string(),
                row.hostname.clone().unwrap_or_default(),
            ];
            conns.write_record(record).map_err(csv_to_io)
        })
        .map_err(callback_err)?;

    let wm_new_sessions = store
        .for_each_session_after(wm_sessions, &mut |_: i64, s: &AppSession| {
            n_sess += 1;
            let record = [
                rfc3339(s.ts_start_ms, offset),
                rfc3339(s.ts_end_ms, offset),
                (s.ts_end_ms.saturating_sub(s.ts_start_ms) / 1000).to_string(),
                s.process_name.clone(),
                s.window_title.clone(),
            ];
            usage.write_record(record).map_err(csv_to_io)
        })
        .map_err(callback_err)?;

    let wm_new_titles = store
        .for_each_title_after(wm_titles, &mut |_: i64, t: &WindowTitle| {
            n_title += 1;
            let record = [
                rfc3339(t.ts_ms, offset),
                t.process_name.clone(),
                t.title.clone(),
            ];
            titles.write_record(record).map_err(csv_to_io)
        })
        .map_err(callback_err)?;

    conns.flush().map_err(ExportError::Io)?;
    usage.flush().map_err(ExportError::Io)?;
    titles.flush().map_err(ExportError::Io)?;

    // The summarized range covers the smallest ts of anything exported (0 for
    // an empty first export) through the export instant itself.
    let from_ms = min_ts.unwrap_or(0);
    let filter = ConnFilter {
        from_ms,
        to_ms: now_ms,
        ..ConnFilter::default()
    };
    let overview = store.overview(&filter).map_err(ExportError::Storage)?;
    let summary = SummaryJson {
        total_connections: overview.total_connections,
        top_apps: overview.top_apps,
        top_services: overview.top_services,
        range: RangeJson {
            from_ms,
            to_ms: now_ms,
        },
    };
    write_json(dir, "summary.json", &summary)?;

    // Meta counters are the agent's persistence, not the in-process ones: on
    // a fresh database every key is absent and exports as 0 (flag F4).
    let counters = std::collections::BTreeMap::from_iter(
        COUNTER_KEYS
            .iter()
            .map(|key| {
                let value = store
                    .get_meta(key)
                    .map_err(ExportError::Storage)?
                    .and_then(|raw| raw.parse::<u64>().ok())
                    .unwrap_or(0);
                Ok(((*key).to_string(), value))
            })
            .collect::<Result<Vec<_>, ExportError>>()?,
    );
    let manifest = ManifestJson {
        connection_rows: n_conn,
        session_rows: n_sess,
        title_rows: n_title,
        from_ms,
        to_ms: now_ms,
        generated_at_ms: now_ms,
        schema_version: store
            .get_meta("schema_version")
            .map_err(ExportError::Storage)?
            .unwrap_or_else(|| "1".to_string()),
        tool_version: env!("CARGO_PKG_VERSION"),
        counters,
    };
    write_json(dir, "manifest.json", &manifest)?;

    Ok(Staged {
        connection_rows: n_conn,
        session_rows: n_sess,
        title_rows: n_title,
        wm_connections: wm_new_connections,
        wm_sessions: wm_new_sessions,
        wm_titles: wm_new_titles,
    })
}

/// Fresh CSV writer with its fixed header row.
fn new_csv(dir: &Path, file: &str, header: &[&str]) -> Result<csv::Writer<File>, ExportError> {
    let mut wtr = csv::Writer::from_writer(File::create(dir.join(file)).map_err(ExportError::Io)?);
    wtr.write_record(header).map_err(ExportError::Csv)?;
    Ok(wtr)
}

/// Pretty-printed JSON document, closed before returning.
fn write_json<T: Serialize>(dir: &Path, file: &str, value: &T) -> Result<(), ExportError> {
    let file = File::create(dir.join(file)).map_err(ExportError::Io)?;
    serde_json::to_writer_pretty(file, value).map_err(ExportError::Json)?;
    Ok(())
}

/// `now_ms` as `YYYY-MM-DD` in local time (UTC where the local offset is
/// undeterminable).
fn local_date(now_ms: u64) -> String {
    let fmt = format_description!("[year]-[month]-[day]");
    ts_dt(now_ms)
        .to_offset(local_offset())
        .format(&fmt)
        .unwrap_or_default()
}

/// RFC 3339 timestamp with an explicit offset (local offset, UTC fallback).
fn rfc3339(ts_ms: u64, offset: UtcOffset) -> String {
    let fmt = format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second][offset_hour sign:mandatory]:[offset_minute]"
    );
    ts_dt(ts_ms)
        .to_offset(offset)
        .format(&fmt)
        .unwrap_or_default()
}

/// `ts_ms` → `OffsetDateTime` (epoch on implausible values; the `i64` ms
/// range covers realistic timestamps).
fn ts_dt(ts_ms: u64) -> OffsetDateTime {
    let nanos = i128::from(i64::try_from(ts_ms).map_or(0i64, |ms| ms.saturating_mul(1_000_000)));
    OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// Machine-local offset; the `time` crate cannot always detect it (e.g. on
/// some multithreaded Unix platforms), in which case UTC is used.
fn local_offset() -> UtcOffset {
    UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC)
}

/// Meta value parsed as `i64` watermark; absent or unparseable ⇒ 0.
fn meta_i64(store: &Store, key: &str) -> Result<i64, ExportError> {
    Ok(store
        .get_meta(key)
        .map_err(ExportError::Storage)?
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(0))
}

/// Sets `meta[key] = max(existing, id)`; a watermark never moves backwards.
fn bump_watermark(store: &mut Store, key: &str, id: i64) -> Result<(), ExportError> {
    let previous = meta_i64(store, key)?;
    store
        .set_meta(key, &previous.max(id).to_string())
        .map_err(ExportError::Storage)
}

/// Only called after the rename: strict monotonicity of `export_wm_*` plus
/// `last_export = now_ms`. A poisoned writer lock is recovered, not dropped.
fn commit_watermarks(
    writer: &SharedStore,
    now_ms: u64,
    staged: &Staged,
) -> Result<(), ExportError> {
    let mut store = writer.lock().unwrap_or_else(PoisonError::into_inner);
    bump_watermark(&mut store, "export_wm_connections", staged.wm_connections)?;
    bump_watermark(&mut store, "export_wm_sessions", staged.wm_sessions)?;
    bump_watermark(&mut store, "export_wm_titles", staged.wm_titles)?;
    store
        .set_meta("last_export", &now_ms.to_string())
        .map_err(ExportError::Storage)
}

/// A failed CSV record inside a streaming callback surfaces as the io error
/// the callback contract expects.
fn csv_to_io(err: csv::Error) -> io::Error {
    io::Error::other(err)
}

/// The streaming readers wrap callback failures in `StorageError::Callback`;
/// for the caller that is an ordinary io failure of this export.
fn callback_err(err: StorageError) -> ExportError {
    match err {
        StorageError::Callback(io) => ExportError::Io(io),
        other => other.into(),
    }
}
