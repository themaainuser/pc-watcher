//! Export tests (plan Task 7): hostile CSV fields, manifest contents, the
//! incremental id-watermark behaviour across two exports, folder naming with
//! the same-day `-2` suffix, and failure modes that keep watermarks intact.
//!
//! Every test uses a temp-FILE database (never `:memory:`) so the read-only
//! export store reads the writer's WAL state; a writer stays open in the
//! test process the way the real agent keeps one.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pcw_export::{ExportError, run_export};
use pcw_storage::{SharedStore, Store, WriteOp};
use pcw_types::{
    AppSession, COUNTER_KEYS, ConnectionEvent, Direction, EventKind, Protocol, WindowTitle,
};
use serde_json::Value;

/// Fresh temp dir with a seeded writer store and its export root. `_dir`
/// keeps the TempDir (hereby the database file) alive for the whole test.
struct TestEnv {
    _dir: tempfile::TempDir,
    db: PathBuf,
    out: PathBuf,
    writer: SharedStore,
}

fn setup() -> TestEnv {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let out = dir.path().join("export");
    let store = Store::open(&db).unwrap();
    TestEnv {
        _dir: dir,
        db,
        out,
        writer: Arc::new(Mutex::new(store)),
    }
}

fn seed(writer: &SharedStore, ops: &[WriteOp], now_ms: u64) {
    writer.lock().unwrap().apply_batch(ops, now_ms).unwrap();
}

/// A standalone close event: exactly one connections row per event, without
/// touching the writer's open-TCP map.
fn conn(ts_ms: u64, ip: &str, name: &str) -> WriteOp {
    WriteOp::Connection(ConnectionEvent {
        ts_ms,
        kind: EventKind::Close,
        pid: 500,
        process_name: name.to_string(),
        process_path: format!(r"C:\tests\{name}"),
        remote_ip: ip.to_string(),
        remote_port: 443,
        local_port: 51_000,
        protocol: Protocol::Tcp,
        direction: Direction::Out,
    })
}

fn session(ts_start: u64, ts_end: u64, title: &str) -> WriteOp {
    WriteOp::Session(AppSession {
        ts_start_ms: ts_start,
        ts_end_ms: ts_end,
        process_name: "app.exe".to_string(),
        window_title: title.to_string(),
    })
}

fn title(ts_ms: u64, text: &str) -> WriteOp {
    WriteOp::Title(WindowTitle {
        ts_ms,
        process_name: "app.exe".to_string(),
        title: text.to_string(),
    })
}

/// Names of the export folders that exist under `out`, sorted.
fn folders(out: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(out)
        .unwrap()
        .filter_map(|entry| {
            entry
                .ok()
                .map(|e| e.file_name().to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names
}

/// All data rows (header excluded) of one exported CSV as field vectors.
fn records(folder: &Path, file: &str) -> Vec<Vec<String>> {
    let mut rdr = csv::Reader::from_path(folder.join(file)).unwrap();
    rdr.records()
        .map(|record| {
            record
                .unwrap()
                .iter()
                .map(|field| field.to_string())
                .collect()
        })
        .collect()
}

/// True iff `ts` looks like `yyyy-mm-ddThh:mm:ss+hh:mm` (RFC 3339 with an
/// explicit offset; the export falls back to `+00:00` only where the local
/// offset is undeterminable).
fn is_rfc3339_local(ts: &str) -> bool {
    if ts.len() != 25 {
        return false;
    }
    for (i, b) in ts.bytes().enumerate() {
        let separator = matches!(
            (i, b),
            (4, b'-')
                | (7, b'-')
                | (10, b'T')
                | (13, b':')
                | (16, b':')
                | (19, b'+' | b'-')
                | (22, b':')
        );
        if !separator && !b.is_ascii_digit() {
            return false;
        }
    }
    true
}

#[test]
fn hostile_window_title_survives_csv_roundtrip() {
    let TestEnv {
        _dir,
        db,
        out,
        writer,
    } = setup();
    // Tab, quote, comma and a raw newline inside one field, seeded as both an
    // app-session and a window-title row (Review Focus #4).
    let hostile = "He said \"hi\",\ta\nnext line";
    seed(
        &writer,
        &[session(1000, 2500, hostile), title(1500, hostile)],
        3000,
    );
    run_export(&db, &writer, &out, 4000).unwrap();

    let folder = out.join(&folders(&out)[0]);

    let usage = records(&folder, "app-usage.csv");
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].len(), 5);
    assert_eq!(usage[0][2], "1", "duration_secs truncates 1500 ms to 1 s");
    assert_eq!(usage[0][3], "app.exe");
    assert_eq!(usage[0][4], hostile, "title must come back byte-identical");
    assert!(
        is_rfc3339_local(&usage[0][0]) && is_rfc3339_local(&usage[0][1]),
        "timestamps must be RFC 3339 local, got {:?}",
        &usage[0][0..2]
    );

    let titles = records(&folder, "window-titles.csv");
    assert_eq!(titles.len(), 1);
    assert_eq!(titles[0].len(), 3);
    assert_eq!(titles[0][1], "app.exe");
    assert_eq!(titles[0][2], hostile, "title must come back byte-identical");
    assert!(is_rfc3339_local(&titles[0][0]));
}

#[test]
fn manifest_carries_counts_counters_and_version() {
    let TestEnv {
        _dir,
        db,
        out,
        writer,
    } = setup();
    seed(
        &writer,
        &[
            conn(1000, "1.1.1.1", "a.exe"),
            session(1000, 2000, "t"),
            title(1500, "t"),
        ],
        2000,
    );
    // A fresh install has no counters until the agent persists one; seed just
    // `udp_capped` and check the rest default to 0.
    writer.lock().unwrap().set_meta("udp_capped", "7").unwrap();
    run_export(&db, &writer, &out, 4000).unwrap();

    let folder = out.join(&folders(&out)[0]);
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(folder.join("manifest.json")).unwrap()).unwrap();
    let obj = manifest.as_object().unwrap();
    assert_eq!(obj.len(), 9, "manifest keys: {obj:?}");

    assert_eq!(manifest["connection_rows"].as_u64(), Some(1));
    assert_eq!(manifest["session_rows"].as_u64(), Some(1));
    assert_eq!(manifest["title_rows"].as_u64(), Some(1));
    assert_eq!(manifest["generated_at_ms"].as_u64(), Some(4000));
    assert_eq!(manifest["schema_version"].as_str(), Some("1"));
    assert_eq!(
        manifest["tool_version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );

    let counters = manifest["counters"].as_object().unwrap();
    assert_eq!(counters.len(), COUNTER_KEYS.len());
    for key in COUNTER_KEYS {
        assert!(counters.contains_key(key), "missing counter key {key}");
    }
    assert_eq!(counters["udp_capped"].as_u64(), Some(7));
    assert_eq!(
        counters["watchdog_restarts"].as_u64(),
        Some(0),
        "missing meta counters export as 0"
    );
}

#[test]
fn rows_written_after_export_go_in_next_export() {
    let TestEnv {
        _dir,
        db,
        out,
        writer,
    } = setup();
    seed(
        &writer,
        &[conn(1_699_000_000_000, "8.8.8.8", "first.exe")],
        2000,
    );
    run_export(&db, &writer, &out, 1_700_000_000_000).unwrap();

    // ts_first OLDER than the first export's now_ms — but a higher row id, so
    // an id-based watermark must still pick it up (Review Focus #1).
    seed(
        &writer,
        &[conn(1_700_000_000_000 - 500_000, "9.9.9.9", "late.exe")],
        2_000,
    );
    run_export(&db, &writer, &out, 1_700_000_060_000).unwrap();

    let names = folders(&out);
    assert_eq!(names.len(), 2, "expected two export folders, got {names:?}");
    assert_eq!(
        names[1],
        format!("{}-2", names[0]),
        "both nows fall on the same local day, so the second folder is a -2 suffix"
    );
    let (first, second) = (out.join(&names[0]), out.join(&names[1]));

    let first_rows = records(&first, "connections.csv");
    assert_eq!(first_rows.len(), 1, "first export ships full history");
    assert_eq!(first_rows[0][4], "8.8.8.8", "remote_ip column");

    let second_rows = records(&second, "connections.csv");
    assert_eq!(
        second_rows.len(),
        1,
        "second export must ship exactly the late row"
    );
    assert_eq!(second_rows[0][4], "9.9.9.9");
    assert_eq!(second_rows[0][2], "late.exe");
}

#[test]
fn watermarks_and_last_export_set_after_success() {
    let TestEnv {
        _dir,
        db,
        out,
        writer,
    } = setup();
    seed(
        &writer,
        &[
            conn(1000, "1.1.1.1", "a.exe"),
            conn(2000, "2.2.2.2", "b.exe"),
            session(1000, 2000, "t"),
            title(1500, "t"),
        ],
        2000,
    );
    run_export(&db, &writer, &out, 1_700_000_000_000).unwrap();

    let store = writer.lock().unwrap();
    assert_eq!(
        store.get_meta("export_wm_connections").unwrap().as_deref(),
        Some("2"),
        "highest exported connection id"
    );
    assert_eq!(
        store.get_meta("export_wm_sessions").unwrap().as_deref(),
        Some("1")
    );
    assert_eq!(
        store.get_meta("export_wm_titles").unwrap().as_deref(),
        Some("1")
    );
    assert_eq!(
        store.get_meta("last_export").unwrap().as_deref(),
        Some("1700000000000")
    );
}

#[test]
fn missing_export_folder_fails_and_keeps_watermarks() {
    let TestEnv {
        _dir,
        db,
        out,
        writer,
    } = setup();
    seed(&writer, &[conn(1000, "1.1.1.1", "a.exe")], 1000);

    // `nope` does not exist, i.e. the given folder has no existing parent.
    let target = out.join("nope").join("deeper");
    let err = run_export(&db, &writer, &target, 1000).unwrap_err();
    assert!(matches!(err, ExportError::FolderMissing(_)), "got {err:?}");
    assert!(
        !out.exists(),
        "a failed export must not create anything under the export root"
    );

    let store = writer.lock().unwrap();
    for key in [
        "export_wm_connections",
        "export_wm_sessions",
        "export_wm_titles",
        "last_export",
    ] {
        assert_eq!(store.get_meta(key).unwrap(), None, "{key} must stay unset");
    }
}

#[test]
fn summary_json_keys_exact() {
    let TestEnv {
        _dir,
        db,
        out,
        writer,
    } = setup();
    seed(&writer, &[conn(1000, "1.1.1.1", "a.exe")], 1000);
    run_export(&db, &writer, &out, 4000).unwrap();

    let folder = out.join(&folders(&out)[0]);
    let summary: Value =
        serde_json::from_str(&fs::read_to_string(folder.join("summary.json")).unwrap()).unwrap();
    let mut keys: Vec<String> = summary.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["range", "top_apps", "top_services", "total_connections"],
        "summary must have exactly these four top-level keys"
    );
    assert_eq!(summary["total_connections"].as_u64(), Some(1));

    let range = summary["range"].as_object().unwrap();
    assert_eq!(range.len(), 2);
    assert!(range.contains_key("from_ms") && range.contains_key("to_ms"));
}

#[test]
fn second_export_same_day_gets_suffixed_folder() {
    let TestEnv {
        _dir,
        db,
        out,
        writer,
    } = setup();
    seed(&writer, &[conn(1000, "1.1.1.1", "a.exe")], 1000);
    run_export(&db, &writer, &out, 1_700_000_000_000).unwrap();

    let first = folders(&out);
    assert_eq!(first.len(), 1);

    run_export(&db, &writer, &out, 1_700_000_000_000).unwrap();

    let names = folders(&out);
    assert_eq!(names.len(), 2, "both exports must survive, got {names:?}");
    assert_eq!(names[0], first[0], "first folder name untouched");
    assert_eq!(
        names[1],
        format!("{}-2", names[0]),
        "second gets the -2 suffix"
    );
}
