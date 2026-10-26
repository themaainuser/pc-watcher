//! File exports (SDD 2026-10-26-pc-watcher-export, task 2): the two download
//! routes `/api/export/xlsx` and `/api/export/csv` are built here from the
//! same queries the JSON endpoints use — the *full* result sets for the
//! requested range (no UI row cap), filtered with `routes::parse_filter`
//! verbatim.
//!
//! - `/api/export/xlsx` → one workbook, sheets in order `Manifest`,
//!   `Connections`, `Sessions`, `Titles`.
//! - `/api/export/csv` → one zip with exactly `manifest.json`,
//!   `connections.csv`, `sessions.csv`, `titles.csv`.
//! - Both manifests follow the brief: tool version, export timestamp with
//!   local timezone, the range (ms + readable), every filter value (or
//!   "any"), the row count per dataset and the source database path.
//! - Columns follow the actual `pcw-storage` schema: every column each
//!   table holds ships — `connections` includes the `hostname` LEFT JOIN,
//!   sessions/titles go through `session_rows`/`title_rows` so their `id`
//!   ships too.
//!
//! The plan explicitly accepts holding a full-range export (~160k rows,
//! tens of MB) in memory for this local single-user tool; there is
//! deliberately no streaming machinery here.

use std::io::{Cursor, Write};
use std::path::Path;

use rust_xlsxwriter::{Format, Workbook};
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};
use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use pcw_storage::{ConnFilter, ConnectionRow, SessionRow, StorageError, Store, TitleRow};
use pcw_types::{Direction, Protocol};

/// Content types of the two download routes.
pub const XLSX_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
pub const ZIP_CONTENT_TYPE: &str = "application/zip";

/// Every field the reused connection query returns, in row order: the whole
/// `connections` schema (schema v1 has no `service_name` column) plus the
/// joined hostname, exactly like [`ConnectionRow`].
const CONNECTION_HEADER: [&str; 12] = [
    "id",
    "ts_first",
    "ts_last",
    "pid",
    "process_name",
    "process_path",
    "remote_ip",
    "remote_port",
    "local_port",
    "protocol",
    "direction",
    "hostname",
];
/// Every schema column of `app_sessions` in schema order ([`SessionRow`]).
const SESSION_HEADER: [&str; 5] = ["id", "ts_start", "ts_end", "process_name", "window_title"];
/// Every schema column of `window_titles` in schema order ([`TitleRow`]).
const TITLE_HEADER: [&str; 4] = ["id", "ts", "process_name", "title"];

/// The bounded connection list query is shared with the JSON endpoints;
/// "full range" means passing the largest possible cap instead of the UI's
/// row limit.
const FULL_RESULT_LIMIT: u32 = u32::MAX;

/// Anything that can go wrong while building a download. The route layer
/// answers every variant with the same retryable response, so the kind only
/// matters for test/debug clarity.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("xlsx error: {0}")]
    Xlsx(#[from] rust_xlsxwriter::XlsxError),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// One manifest entry. `Num` values stay numeric in `manifest.json`; in the
/// xlsx Manifest sheet every value cell is written as text (a cell number
/// cannot represent `u64::MAX` without losing precision).
enum ManifestVal {
    Num(u64),
    Text(String),
}

/// The complete manifest, key/value pairs in display order — the brief's
/// fields: version, local-tz timestamp, ms + readable range, every filter
/// (or "any"), per-dataset row counts, source DB path.
fn manifest_entries(
    filter: &ConnFilter,
    counts: (u64, u64, u64),
    db_path: &Path,
) -> Vec<(String, ManifestVal)> {
    let filter_of =
        |value: Option<String>| ManifestVal::Text(value.unwrap_or_else(|| "any".into()));
    let enum_text = |value: Option<&'static str>| ManifestVal::Text(value.unwrap_or("any").into());
    vec![
        (
            "tool_version".into(),
            ManifestVal::Text(env!("CARGO_PKG_VERSION").into()),
        ),
        ("generated_at".into(), ManifestVal::Text(rfc3339(now_ms()))),
        ("from_ms".into(), ManifestVal::Num(filter.from_ms)),
        (
            "from_local".into(),
            ManifestVal::Text(readable_ms(filter.from_ms)),
        ),
        ("to_ms".into(), ManifestVal::Num(filter.to_ms)),
        (
            "to_local".into(),
            ManifestVal::Text(readable_ms(filter.to_ms)),
        ),
        ("filter_app".into(), filter_of(filter.app.clone())),
        (
            "filter_protocol".into(),
            enum_text(filter.protocol.map(Protocol::as_str)),
        ),
        (
            "filter_port".into(),
            match filter.port {
                Some(port) => ManifestVal::Text(port.to_string()),
                None => ManifestVal::Text("any".into()),
            },
        ),
        (
            "filter_direction".into(),
            enum_text(filter.direction.map(Direction::as_str)),
        ),
        ("rows_connections".into(), ManifestVal::Num(counts.0)),
        ("rows_sessions".into(), ManifestVal::Num(counts.1)),
        ("rows_titles".into(), ManifestVal::Num(counts.2)),
        (
            "source_db".into(),
            ManifestVal::Text(db_path.to_string_lossy().into_owned()),
        ),
    ]
}

/// The manifest as pretty JSON for `manifest.json` inside the zip.
fn manifest_json(entries: &[(String, ManifestVal)]) -> Result<Vec<u8>, ExportError> {
    let mut object = serde_json::Map::new();
    for (key, value) in entries {
        object.insert(
            key.clone(),
            match value {
                ManifestVal::Num(number) => serde_json::Value::from(*number),
                ManifestVal::Text(text) => serde_json::Value::from(text.as_str()),
            },
        );
    }
    let mut json = serde_json::to_vec_pretty(&serde_json::Value::Object(object))?;
    json.push(b'\n');
    Ok(json)
}

/// One CSV byte buffer with its fixed header row. Fields with commas,
/// quotes and newlines are quoted by the csv crate.
fn csv_bytes(header: &[&str], records: Vec<Vec<String>>) -> Result<Vec<u8>, ExportError> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(header)?;
    for record in records {
        writer.write_record(record)?;
    }
    Ok(writer.into_inner().map_err(|err| err.into_error())?)
}

/// The three datasets for one request: full result sets, same queries and
/// filters as the JSON endpoints' ranges — connections also carries the
/// hostname join, sessions/titles their whole schema (including `id`).
struct ExportData {
    connections: Vec<ConnectionRow>,
    sessions: Vec<SessionRow>,
    titles: Vec<TitleRow>,
}

fn fetch(store: &Store, filter: &ConnFilter) -> Result<ExportData, StorageError> {
    Ok(ExportData {
        connections: store.connections(filter, FULL_RESULT_LIMIT)?,
        sessions: store.session_rows(filter.from_ms, filter.to_ms, filter.app.as_deref())?,
        titles: store.title_rows(filter.from_ms, filter.to_ms, filter.app.as_deref())?,
    })
}

/// Connection rows as CSV fields, enums lowercase like every other text
/// output of this workspace's exports.
fn connection_record(row: &ConnectionRow) -> Vec<String> {
    vec![
        row.id.to_string(),
        row.ts_first_ms.to_string(),
        row.ts_last_ms.to_string(),
        row.pid.to_string(),
        row.process_name.clone(),
        row.process_path.clone(),
        row.remote_ip.clone(),
        row.remote_port.to_string(),
        row.local_port.to_string(),
        row.protocol.as_str().into(),
        row.direction.as_str().into(),
        row.hostname.clone().unwrap_or_default(),
    ]
}

/// Builds the `/api/export/xlsx` download: Manifest, Connections, Sessions,
/// Titles — in that sheet order.
pub fn export_xlsx(
    store: &Store,
    filter: &ConnFilter,
    db_path: &Path,
) -> Result<Vec<u8>, ExportError> {
    let data = fetch(store, filter)?;
    let entries = manifest_entries(
        filter,
        (
            data.connections.len() as u64,
            data.sessions.len() as u64,
            data.titles.len() as u64,
        ),
        db_path,
    );

    let mut workbook = Workbook::new();
    let bold = Format::new().set_bold();

    let manifest = workbook.add_worksheet();
    manifest.set_name("Manifest")?;
    manifest.write_row_with_format(0, 0, ["field", "value"], &bold)?;
    for (i, (key, value)) in entries.iter().enumerate() {
        let row = (i + 1) as u32;
        manifest.write_string(row, 0, key.as_str())?;
        let text = match value {
            // Text only: a numeric cell cannot hold `u64::MAX` exactly.
            ManifestVal::Num(number) => number.to_string(),
            ManifestVal::Text(text) => text.clone(),
        };
        manifest.write_string(row, 1, text)?;
    }

    let connections = workbook.add_worksheet();
    connections.set_name("Connections")?;
    connections.write_row_with_format(0, 0, CONNECTION_HEADER, &bold)?;
    for (r, row) in data.connections.iter().enumerate() {
        let row_num = (r + 1) as u32;
        // Epoch-ms values (~1.7e12) stay exact in f64's 2^53 integer range.
        connections.write_number(row_num, 0, row.id as f64)?;
        connections.write_number(row_num, 1, row.ts_first_ms as f64)?;
        connections.write_number(row_num, 2, row.ts_last_ms as f64)?;
        connections.write_number(row_num, 3, f64::from(row.pid))?;
        connections.write_string(row_num, 4, &row.process_name)?;
        connections.write_string(row_num, 5, &row.process_path)?;
        connections.write_string(row_num, 6, &row.remote_ip)?;
        connections.write_number(row_num, 7, f64::from(row.remote_port))?;
        connections.write_number(row_num, 8, f64::from(row.local_port))?;
        connections.write_string(row_num, 9, row.protocol.as_str())?;
        connections.write_string(row_num, 10, row.direction.as_str())?;
        if let Some(hostname) = &row.hostname {
            connections.write_string(row_num, 11, hostname)?;
        } // None stays a blank cell, like the NULL it came from.
    }

    let sessions = workbook.add_worksheet();
    sessions.set_name("Sessions")?;
    sessions.write_row_with_format(0, 0, SESSION_HEADER, &bold)?;
    for (r, session) in data.sessions.iter().enumerate() {
        let row = (r + 1) as u32;
        sessions.write_number(row, 0, session.id as f64)?;
        sessions.write_number(row, 1, session.ts_start_ms as f64)?;
        sessions.write_number(row, 2, session.ts_end_ms as f64)?;
        sessions.write_string(row, 3, &session.process_name)?;
        sessions.write_string(row, 4, &session.window_title)?;
    }

    let titles = workbook.add_worksheet();
    titles.set_name("Titles")?;
    titles.write_row_with_format(0, 0, TITLE_HEADER, &bold)?;
    for (r, title) in data.titles.iter().enumerate() {
        let row = (r + 1) as u32;
        titles.write_number(row, 0, title.id as f64)?;
        titles.write_number(row, 1, title.ts_ms as f64)?;
        titles.write_string(row, 2, &title.process_name)?;
        titles.write_string(row, 3, &title.title)?;
    }

    Ok(workbook.save_to_buffer()?)
}

/// Builds the `/api/export/csv` download: a zip of `manifest.json`,
/// `connections.csv`, `sessions.csv` and `titles.csv` — those four files
/// and nothing else.
pub fn export_zip(
    store: &Store,
    filter: &ConnFilter,
    db_path: &Path,
) -> Result<Vec<u8>, ExportError> {
    let data = fetch(store, filter)?;
    let entries = manifest_entries(
        filter,
        (
            data.connections.len() as u64,
            data.sessions.len() as u64,
            data.titles.len() as u64,
        ),
        db_path,
    );
    let connections = csv_bytes(
        &CONNECTION_HEADER,
        data.connections.iter().map(connection_record).collect(),
    )?;
    let sessions = csv_bytes(
        &SESSION_HEADER,
        data.sessions
            .iter()
            .map(|s| {
                vec![
                    s.id.to_string(),
                    s.ts_start_ms.to_string(),
                    s.ts_end_ms.to_string(),
                    s.process_name.clone(),
                    s.window_title.clone(),
                ]
            })
            .collect(),
    )?;
    let titles = csv_bytes(
        &TITLE_HEADER,
        data.titles
            .iter()
            .map(|t| {
                vec![
                    t.id.to_string(),
                    t.ts_ms.to_string(),
                    t.process_name.clone(),
                    t.title.clone(),
                ]
            })
            .collect(),
    )?;

    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, bytes) in [
        ("manifest.json", manifest_json(&entries)?),
        ("connections.csv", connections),
        ("sessions.csv", sessions),
        ("titles.csv", titles),
    ] {
        archive.start_file(name, options)?;
        archive.write_all(&bytes)?;
    }
    Ok(archive.finish()?.into_inner())
}

/// Today's local date as `YYYY-MM-DD` (UTC fallback where the offset is
/// undeterminable) for the Content-Disposition filename.
pub fn today() -> String {
    let format = format_description!("[year]-[month]-[day]");
    OffsetDateTime::now_utc()
        .to_offset(local_offset())
        .format(&format)
        .unwrap_or_default()
}

/// RFC 3339 timestamp with an explicit local offset (UTC fallback), same
/// shape as `pcw_export`'s CSV timestamps.
fn rfc3339(ts_ms: u64) -> String {
    let format = format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second][offset_hour sign:mandatory]:[offset_minute]"
    );
    OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(i64::try_from(ts_ms).unwrap_or(i64::MAX)) * 1_000_000,
    )
    .unwrap_or(OffsetDateTime::UNIX_EPOCH)
    .to_offset(local_offset())
    .format(&format)
    .unwrap_or_default()
}

/// A range edge as readable text. Bounds beyond the representable range
/// (the default `to_ms = u64::MAX`) degrade to the plain number.
fn readable_ms(ts_ms: u64) -> String {
    if ts_ms > i64::MAX as u64 {
        return ts_ms.to_string();
    }
    rfc3339(ts_ms)
}

/// Machine-local offset; `time` cannot always detect it, in which case UTC
/// is used (same fallback as `pcw_export`).
fn local_offset() -> UtcOffset {
    UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC)
}

fn now_ms() -> u64 {
    u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}
