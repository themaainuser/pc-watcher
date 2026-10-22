//! Retention-pruner tests (plan Task 6): watermark-guarded, chunked deletes
//! across `connections`, `app_sessions` and `window_titles`, with the
//! `hostnames` cache never touched. Rows are seeded directly through the
//! test-only connection handle on temp-FILE databases (never `:memory:`).

use std::time::{SystemTime, UNIX_EPOCH};

use pcw_storage::Store;
use rusqlite::params;

const DAY_MS: u64 = 86_400_000;
const WM_CONNECTIONS: &str = "export_wm_connections";
const WM_SESSIONS: &str = "export_wm_sessions";
const WM_TITLES: &str = "export_wm_titles";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Opens a fresh temp-FILE database (WAL requires a real file).
fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data.db")).unwrap();
    (dir, store)
}

/// Sets all three export watermarks to the same value.
fn set_watermarks(store: &mut Store, value: &str) {
    for key in [WM_CONNECTIONS, WM_SESSIONS, WM_TITLES] {
        store.set_meta(key, value).unwrap();
    }
}

/// Inserts `n` rows into one of the three prunable event tables, all with
/// the same timestamp; ids run 1..=n.
fn seed_table(store: &mut Store, table: &str, n: i64, ts: i64) {
    let conn = store.conn_for_tests();
    for _ in 0..n {
        let done = match table {
            "connections" => conn.execute(
                "INSERT INTO connections(ts_first, ts_last, pid, process_name, process_path,
                                         remote_ip, remote_port, local_port, protocol, direction)
                 VALUES (?1, ?1, 1, 'a.exe', 'C:\\tests\\a.exe', '1.1.1.1', 443, 55000, 'tcp', 'out')",
                params![ts],
            ),
            "app_sessions" => conn.execute(
                "INSERT INTO app_sessions(ts_start, ts_end, process_name, window_title)
                 VALUES (?1, ?1, 'a.exe', 'title')",
                params![ts],
            ),
            "window_titles" => conn.execute(
                "INSERT INTO window_titles(ts, process_name, title) VALUES (?1, 'a.exe', 'title')",
                params![ts],
            ),
            other => unreachable!("not a prunable table: {other}"),
        };
        done.unwrap();
    }
}

fn count(store: &mut Store, table: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    store
        .conn_for_tests()
        .query_row(&sql, [], |r| r.get(0))
        .unwrap()
}

fn ids(store: &mut Store, table: &str) -> Vec<i64> {
    let sql = format!("SELECT id FROM {table} ORDER BY id");
    store
        .conn_for_tests()
        .prepare(&sql)
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<i64>, _>>()
        .unwrap()
}

/// Four hostnames rows covering every source value.
fn seed_hostnames(store: &mut Store) {
    let conn = store.conn_for_tests();
    conn.execute(
        "INSERT INTO hostnames(ip, hostname, source, resolved_at)
         VALUES ('1.1.1.1', 'one.one.one.one', 'dns', 100)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO hostnames(ip, hostname, source, resolved_at) VALUES ('2.2.2.2', NULL, 'pending', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO hostnames(ip, hostname, source, resolved_at) VALUES ('3.3.3.3', NULL, 'none', 200)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO hostnames(ip, hostname, source, resolved_at)
         VALUES ('4.4.4.4', 'four.example.test', 'ptr', 300)",
        [],
    )
    .unwrap();
}

fn hostnames_rows(store: &mut Store) -> Vec<(String, Option<String>, String, i64)> {
    store
        .conn_for_tests()
        .prepare("SELECT ip, hostname, source, resolved_at FROM hostnames ORDER BY ip")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<Vec<(String, Option<String>, String, i64)>, _>>()
        .unwrap()
}

/// Rows 11 days old with the watermarks at the max ids: everything is
/// behind the export point and past the retention cutoff, so one step
/// deletes all 9 rows (3 per table) and the tables end up empty.
#[test]
fn deletes_old_exported_rows() {
    let (_dir, mut store) = open();
    let now = now_ms();
    let ts = (now - 11 * DAY_MS) as i64;
    seed_table(&mut store, "connections", 3, ts);
    seed_table(&mut store, "app_sessions", 3, ts);
    seed_table(&mut store, "window_titles", 3, ts);
    set_watermarks(&mut store, "3");

    let deleted = store.prune_step(10, now, 1000).unwrap();
    assert_eq!(deleted, 9);
    assert_eq!(count(&mut store, "connections"), 0);
    assert_eq!(count(&mut store, "app_sessions"), 0);
    assert_eq!(count(&mut store, "window_titles"), 0);
}

/// Review Focus #1: rows 11 days old with ids 1..=5 per table and a
/// watermark of 3 — ids 4 and 5 were written after the last export, so the
/// pruner must leave them alone even though they are past retention.
#[test]
fn keeps_old_rows_written_after_export() {
    let (_dir, mut store) = open();
    let now = now_ms();
    let ts = (now - 11 * DAY_MS) as i64;
    seed_table(&mut store, "connections", 5, ts);
    seed_table(&mut store, "app_sessions", 5, ts);
    seed_table(&mut store, "window_titles", 5, ts);
    set_watermarks(&mut store, "3");

    let deleted = store.prune_step(10, now, 1000).unwrap();
    assert_eq!(deleted, 9, "ids 1..=3 delete in every table (3 x 3 tables)");
    assert_eq!(ids(&mut store, "connections"), vec![4, 5]);
    assert_eq!(ids(&mut store, "app_sessions"), vec![4, 5]);
    assert_eq!(ids(&mut store, "window_titles"), vec![4, 5]);
}

/// No export ever happened (no watermark meta, or a corrupt value that does
/// not parse as i64): the watermark is 0, matching no id, so rows stay even
/// when they are years past their retention window.
#[test]
fn without_export_prunes_nothing() {
    let (_dir, mut store) = open();
    let now = now_ms();
    let ts = (now - 730 * DAY_MS) as i64; // two years old
    seed_table(&mut store, "connections", 2, ts);
    seed_table(&mut store, "app_sessions", 2, ts);
    seed_table(&mut store, "window_titles", 2, ts);

    // No watermark meta at all.
    assert_eq!(store.prune_step(10, now, 1000).unwrap(), 0);
    assert_eq!(count(&mut store, "connections"), 2);
    assert_eq!(count(&mut store, "app_sessions"), 2);
    assert_eq!(count(&mut store, "window_titles"), 2);

    // Corrupt ("abc") watermarks parse as 0 → still nothing deleted.
    set_watermarks(&mut store, "abc");
    assert_eq!(store.prune_step(10, now, 1000).unwrap(), 0);
    assert_eq!(count(&mut store, "connections"), 2);
    assert_eq!(count(&mut store, "app_sessions"), 2);
    assert_eq!(count(&mut store, "window_titles"), 2);
}

/// 25 eligible rows (9 connections, 8 sessions, 8 titles) with a shared
/// budget of 10 per step: the three tables share one budget, so the steps
/// return 10, 10, 5 and then 0.
#[test]
fn respects_chunk_size() {
    let (_dir, mut store) = open();
    let now = now_ms();
    let ts = (now - 11 * DAY_MS) as i64;
    seed_table(&mut store, "connections", 9, ts);
    seed_table(&mut store, "app_sessions", 8, ts);
    seed_table(&mut store, "window_titles", 8, ts);
    set_watermarks(&mut store, "999");

    assert_eq!(store.prune_step(10, now, 10).unwrap(), 10);
    assert_eq!(store.prune_step(10, now, 10).unwrap(), 10);
    assert_eq!(store.prune_step(10, now, 10).unwrap(), 5);
    assert_eq!(store.prune_step(10, now, 10).unwrap(), 0);

    assert_eq!(count(&mut store, "connections"), 0);
    assert_eq!(count(&mut store, "app_sessions"), 0);
    assert_eq!(count(&mut store, "window_titles"), 0);
}

/// A full prune down to zero rows leaves every `hostnames` row untouched,
/// whatever its source ('dns', 'pending', 'none', 'ptr').
#[test]
fn never_prunes_hostname_cache() {
    let (_dir, mut store) = open();
    let now = now_ms();
    let ts = (now - 11 * DAY_MS) as i64;
    seed_table(&mut store, "connections", 2, ts);
    seed_table(&mut store, "app_sessions", 2, ts);
    seed_table(&mut store, "window_titles", 2, ts);
    seed_hostnames(&mut store);
    set_watermarks(&mut store, "999");

    let mut total = 0;
    loop {
        let deleted = store.prune_step(10, now, 3).unwrap();
        if deleted == 0 {
            break;
        }
        total += deleted;
    }
    assert_eq!(total, 6, "all event rows pruned");
    assert_eq!(count(&mut store, "connections"), 0);
    assert_eq!(count(&mut store, "app_sessions"), 0);
    assert_eq!(count(&mut store, "window_titles"), 0);

    assert_eq!(
        hostnames_rows(&mut store),
        vec![
            (
                "1.1.1.1".to_string(),
                Some("one.one.one.one".to_string()),
                "dns".to_string(),
                100
            ),
            ("2.2.2.2".to_string(), None, "pending".to_string(), 0),
            ("3.3.3.3".to_string(), None, "none".to_string(), 200),
            (
                "4.4.4.4".to_string(),
                Some("four.example.test".to_string()),
                "ptr".to_string(),
                300
            ),
        ],
        "the hostname cache survives a full prune"
    );
}
