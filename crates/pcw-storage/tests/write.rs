//! Write-path tests (plan Task 4): schema v1, TCP open/close lifecycle, UDP
//! rows, DNS naming of seen IPs, meta helpers, read-only mode, disk-full.

use std::path::PathBuf;

use pcw_storage::{StorageError, Store, WriteOp};
use pcw_types::{
    AppSession, ConnectionEvent, Direction, DnsAnswer, EventKind, Protocol, WindowTitle,
};

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let store = Store::open(&db).unwrap();
    (dir, store)
}

fn tcp(kind: EventKind, ts: u64, local_port: u16, ip: &str, port: u16) -> WriteOp {
    WriteOp::Connection(ConnectionEvent {
        ts_ms: ts,
        kind,
        pid: 100,
        process_name: "test.exe".to_string(),
        process_path: r"C:\test.exe".to_string(),
        remote_ip: ip.to_string(),
        remote_port: port,
        local_port,
        protocol: Protocol::Tcp,
        direction: Direction::Out,
    })
}

fn udp(ts: u64, ip: &str, port: u16) -> WriteOp {
    WriteOp::Connection(ConnectionEvent {
        ts_ms: ts,
        kind: EventKind::Packet,
        pid: 100,
        process_name: "test.exe".to_string(),
        process_path: r"C:\test.exe".to_string(),
        remote_ip: ip.to_string(),
        remote_port: port,
        local_port: 51234,
        protocol: Protocol::Udp,
        direction: Direction::Out,
    })
}

fn dns(name: &str, ips: &[&str], ts: u64) -> WriteOp {
    WriteOp::Dns(DnsAnswer {
        ts_ms: ts,
        name: name.to_string(),
        ips: ips.iter().map(|ip| ip.to_string()).collect(),
    })
}

fn count(store: &mut Store, table: &str) -> u64 {
    let sql = format!("SELECT COUNT(*) FROM \"{table}\"");
    store
        .conn_for_tests()
        .query_row(&sql, [], |r| r.get::<_, i64>(0))
        .unwrap() as u64
}

#[test]
fn open_creates_schema_and_version() {
    let (_dir, mut store) = store();
    assert_eq!(
        store.get_meta("schema_version").unwrap().as_deref(),
        Some("1")
    );

    let conn = store.conn_for_tests();
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table'
               AND name IN ('connections', 'app_sessions', 'window_titles', 'hostnames', 'meta')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 5);
    let indexes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexes, 7);
}

#[test]
fn tcp_open_then_close_is_one_row() {
    let (_dir, mut store) = store();
    store
        .apply_batch(&[tcp(EventKind::Open, 1000, 1234, "1.2.3.4", 80)], 1000)
        .unwrap();
    store
        .apply_batch(&[tcp(EventKind::Close, 5000, 1234, "1.2.3.4", 80)], 5000)
        .unwrap();

    assert_eq!(count(&mut store, "connections"), 1);
    let (first, last): (i64, i64) = store
        .conn_for_tests()
        .query_row("SELECT ts_first, ts_last FROM connections", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((first, last), (1000, 5000));
}

#[test]
fn close_frees_key_so_reconnect_is_new_row() {
    let (_dir, mut store) = store();
    store
        .apply_batch(&[tcp(EventKind::Open, 1000, 1234, "1.2.3.4", 80)], 1000)
        .unwrap();
    store
        .apply_batch(&[tcp(EventKind::Close, 2000, 1234, "1.2.3.4", 80)], 2000)
        .unwrap();
    store
        .apply_batch(&[tcp(EventKind::Open, 3000, 1234, "1.2.3.4", 80)], 3000)
        .unwrap();

    assert_eq!(count(&mut store, "connections"), 2);
}

#[test]
fn close_without_open_inserts_row() {
    let (_dir, mut store) = store();
    store
        .apply_batch(&[tcp(EventKind::Close, 7000, 1234, "1.2.3.4", 80)], 7000)
        .unwrap();

    assert_eq!(count(&mut store, "connections"), 1);
    let (first, last): (i64, i64) = store
        .conn_for_tests()
        .query_row("SELECT ts_first, ts_last FROM connections", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((first, last), (7000, 7000));
}

#[test]
fn udp_packets_never_merge() {
    let (_dir, mut store) = store();
    store
        .apply_batch(&[udp(1000, "9.9.9.9", 53)], 1000)
        .unwrap();
    store
        .apply_batch(&[udp(1000, "9.9.9.9", 53)], 1000)
        .unwrap();

    assert_eq!(count(&mut store, "connections"), 2);
}

#[test]
fn new_ip_gets_pending_row_and_dns_answer_names_it() {
    let (_dir, mut store) = store();

    // Batch 1: first Packet to 9.9.9.9 → pending hostnames row.
    store
        .apply_batch(&[udp(1000, "9.9.9.9", 53)], 1000)
        .unwrap();
    let (hostname, source, resolved_at): (Option<String>, String, i64) = store
        .conn_for_tests()
        .query_row(
            "SELECT hostname, source, resolved_at FROM hostnames WHERE ip = '9.9.9.9'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(hostname, None);
    assert_eq!(source, "pending");
    assert_eq!(resolved_at, 0);

    // Batch 2: DNS answer upgrades the existing row to source 'dns'.
    store
        .apply_batch(&[dns("dns.quad9.net", &["9.9.9.9"], 2000)], 2000)
        .unwrap();
    let (hostname, source, resolved_at): (Option<String>, String, i64) = store
        .conn_for_tests()
        .query_row(
            "SELECT hostname, source, resolved_at FROM hostnames WHERE ip = '9.9.9.9'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(hostname.as_deref(), Some("dns.quad9.net"));
    assert_eq!(source, "dns");
    assert_eq!(resolved_at, 2000);

    // Batch 3: DNS known before first connection → row is 'dns' immediately.
    store
        .apply_batch(&[dns("one.one.one.one", &["1.1.1.1"], 3000)], 3000)
        .unwrap();
    store
        .apply_batch(&[udp(4000, "1.1.1.1", 443)], 4000)
        .unwrap();
    let (hostname, source): (Option<String>, String) = store
        .conn_for_tests()
        .query_row(
            "SELECT hostname, source FROM hostnames WHERE ip = '1.1.1.1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(hostname.as_deref(), Some("one.one.one.one"));
    assert_eq!(source, "dns");
}

#[test]
fn dns_answer_for_unrecorded_ip_stores_nothing() {
    let (_dir, mut store) = store();
    store
        .apply_batch(&[dns("dns.google", &["8.8.8.8"], 1000)], 1000)
        .unwrap();

    assert_eq!(count(&mut store, "hostnames"), 0);
}

#[test]
fn sessions_and_titles_persist_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    {
        let mut store = Store::open(&db).unwrap();
        store
            .apply_batch(
                &[
                    WriteOp::Session(AppSession {
                        ts_start_ms: 1000,
                        ts_end_ms: 2000,
                        process_name: "app.exe".to_string(),
                        window_title: "win".to_string(),
                    }),
                    WriteOp::Title(WindowTitle {
                        ts_ms: 1500,
                        process_name: "app.exe".to_string(),
                        title: "hello".to_string(),
                    }),
                ],
                2000,
            )
            .unwrap();
    }

    let mut reopened = Store::open(&db).unwrap();
    assert_eq!(count(&mut reopened, "app_sessions"), 1);
    assert_eq!(count(&mut reopened, "window_titles"), 1);
    let (title, process_name): (String, String) = reopened
        .conn_for_tests()
        .query_row("SELECT title, process_name FROM window_titles", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(title, "hello");
    assert_eq!(process_name, "app.exe");
}

#[test]
fn readonly_store_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = PathBuf::from(dir.path()).join("data.db");
    let mut store = Store::open(&db_path).unwrap();
    store
        .apply_batch(&[udp(1000, "1.2.3.4", 53)], 1000)
        .unwrap();
    drop(store);

    let mut readonly = Store::open_readonly(&db_path).unwrap();
    let err = readonly
        .apply_batch(&[udp(2000, "5.6.7.8", 53)], 2000)
        .unwrap_err();
    assert!(matches!(err, StorageError::Sqlite(_)), "got {err:?}");
}

#[test]
fn disk_full_is_reported_and_store_stays_usable() {
    let (_dir, mut store) = store();

    {
        let conn = store.conn_for_tests();
        let pages: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(&format!("PRAGMA max_page_count = {};", pages + 1))
            .unwrap();
    }

    let batch: Vec<WriteOp> = (0..5000)
        .map(|i| udp(2000 + i as u64, "10.0.0.1", 5000))
        .collect();
    let err = store.apply_batch(&batch, 7000).unwrap_err();
    assert!(matches!(err, StorageError::DiskFull), "got {err:?}");

    store
        .conn_for_tests()
        .execute_batch("PRAGMA max_page_count = 99999999;")
        .unwrap();
    store
        .apply_batch(
            &[tcp(EventKind::Open, 100_000, 1234, "1.2.3.4", 80)],
            100_000,
        )
        .unwrap();
    assert_eq!(count(&mut store, "connections"), 1);
}
