//! Agent-core tests (plan Task 12): writer flush cadence, low-disk UDP
//! dropping, disk-full survival, crash recovery, counter persistence,
//! timer logic.
//!
//! All stores are seeded from temp-FILE databases (F8): `Store::open` asserts
//! WAL mode, which `:memory:` cannot provide.

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pcw_agent::state;
use pcw_agent::timers::due;
use pcw_agent::writer::{WriterConfig, WriterMsg, run_writer};
use pcw_foreground::OpenSession;
use pcw_storage::{SharedStore, Store, WriteOp};
use pcw_types::{AppSession, ConnectionEvent, Counters, Direction, EventKind, Protocol};

const FREE_BYTES_NEVER_LOW: u64 = u64::MAX;

/// Temp-file-backed store behind the shared handle the writer runs against.
fn store() -> (tempfile::TempDir, SharedStore) {
    let dir = tempfile::tempdir().unwrap();
    let db = Store::open(&dir.path().join("data.db")).unwrap();
    (dir, Arc::new(Mutex::new(db)))
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

fn tcp_open(ts: u64, local_port: u16, ip: &str, port: u16) -> WriteOp {
    WriteOp::Connection(ConnectionEvent {
        ts_ms: ts,
        kind: EventKind::Open,
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

fn count_rows(db: &SharedStore, table: &str) -> u64 {
    let mut guard = db.lock().unwrap();
    let sql = format!("SELECT COUNT(*) FROM \"{table}\"");
    guard
        .conn_for_tests()
        .query_row(&sql, [], |r| r.get::<_, i64>(0))
        .unwrap() as u64
}

fn counter(counters: &Arc<Counters>, key: &str) -> u64 {
    counters
        .snapshot()
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
        .unwrap()
}

/// Polls `f` until it returns true or `timeout` elapses; re-checks once more
/// at the end so a just-passed check is not missed by the sleep granularity.
fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

/// A config whose every check reports the given free bytes.
fn cfg_with_free(min_free_bytes: u64, free: u64) -> WriterConfig {
    WriterConfig {
        min_free_bytes,
        free_bytes: Box::new(move || Some(free)),
        disk_check_every: Duration::ZERO,
    }
}

#[test]
fn writer_flushes_within_1_5s_and_drains_on_shutdown() {
    let (_dir, db) = store();
    let counters = Arc::new(Counters::default());
    let (tx, rx) = channel();
    let handle = std::thread::spawn({
        let db = Arc::clone(&db);
        let counters = Arc::clone(&counters);
        move || run_writer(rx, db, cfg_with_free(0, FREE_BYTES_NEVER_LOW), counters)
    });

    // 3 UDP datagrams: each is its own row.
    for i in 0..3 {
        tx.send(WriterMsg::Op(udp(1000 + i, "9.9.9.9", 5000)))
            .unwrap();
    }
    assert!(
        wait_until(Duration::from_millis(1500), || count_rows(
            &db,
            "connections"
        ) == 3),
        "3 Connection ops not visible within 1.5 s"
    );

    // 2 more, then a clean shutdown: the writer must flush what is queued
    // before returning.
    for i in 3..5 {
        tx.send(WriterMsg::Op(udp(2000 + i, "9.9.9.9", 5000)))
            .unwrap();
    }
    tx.send(WriterMsg::Shutdown).unwrap();
    handle.join().unwrap();
    assert_eq!(count_rows(&db, "connections"), 5);
}

#[test]
fn writer_drops_udp_when_disk_low_and_counts() {
    let (_dir, db) = store();
    let counters = Arc::new(Counters::default());
    let (tx, rx) = channel();
    let handle = std::thread::spawn({
        let db = Arc::clone(&db);
        let counters = Arc::clone(&counters);
        move || run_writer(rx, db, cfg_with_free(2_000_000_000, 1), counters)
    });

    // 4 UDP ops: below the 2 GB threshold, all four are dropped and counted.
    // 1 TCP Open: not a UDP Packet, still attempts a write.
    for i in 0..4 {
        tx.send(WriterMsg::Op(udp(1000 + i, "10.0.0.1", 5000)))
            .unwrap();
    }
    tx.send(WriterMsg::Op(tcp_open(2000, 1234, "10.0.0.2", 443)))
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(3), || count_rows(&db, "connections")
            == 1),
        "TCP Open not written while UDP ops were dropped"
    );
    assert_eq!(counter(&counters, "udp_skipped_low_disk"), 4);

    tx.send(WriterMsg::Shutdown).unwrap();
    handle.join().unwrap();
    assert_eq!(count_rows(&db, "connections"), 1);
}

#[test]
fn writer_survives_disk_full() {
    let (_dir, db) = store();
    {
        let mut guard = db.lock().unwrap();
        let pages: i64 = guard
            .conn_for_tests()
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        guard
            .conn_for_tests()
            .execute_batch(&format!("PRAGMA max_page_count = {};", pages + 1))
            .unwrap();
    }

    let counters = Arc::new(Counters::default());
    let (tx, rx) = channel();
    let handle = std::thread::spawn({
        let db = Arc::clone(&db);
        let counters = Arc::clone(&counters);
        // min_free_bytes = 0: the low-disk path must NOT be what drops these.
        move || run_writer(rx, db, cfg_with_free(0, FREE_BYTES_NEVER_LOW), counters)
    });

    // 5,000 UDP ops, flushed 500 at a time; every batch must hit SQLITE_FULL,
    // be rolled back whole, and have its op count added to the counter.
    for i in 0..5000 {
        tx.send(WriterMsg::Op(udp(1000 + i, "10.9.9.9", 5000)))
            .unwrap();
    }
    assert!(
        wait_until(Duration::from_secs(10), || counter(
            &counters,
            "rows_dropped_disk_full"
        ) > 0),
        "no rows_dropped_disk_full after 5,000 ops"
    );

    tx.send(WriterMsg::Shutdown).unwrap();
    handle.join().unwrap();
    // The whole failed batches are lost: nothing was committed.
    assert_eq!(count_rows(&db, "connections"), 0);
    assert_eq!(counter(&counters, "rows_dropped_disk_full"), 5000);

    // The writer kept running: once the limit is raised, a later op is written.
    {
        let mut guard = db.lock().unwrap();
        guard
            .conn_for_tests()
            .execute_batch("PRAGMA max_page_count = 99999999;")
            .unwrap();
    }
    let (tx, rx) = channel();
    let handle = std::thread::spawn({
        let db = Arc::clone(&db);
        move || run_writer(rx, db, cfg_with_free(0, FREE_BYTES_NEVER_LOW), counters)
    });
    tx.send(WriterMsg::Op(tcp_open(100_000, 1234, "1.2.3.4", 80)))
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(3), || count_rows(&db, "connections")
            == 1),
        "TCP op not written after the page limit was raised"
    );
    tx.send(WriterMsg::Shutdown).unwrap();
    handle.join().unwrap();
}

#[test]
fn open_session_recovery_writes_session_and_clears_meta() {
    let (_dir, db) = store();
    let mut guard = db.lock().unwrap();

    let session = OpenSession {
        ts_start_ms: 1000,
        process_name: "app.exe".to_string(),
        window_title: "win".to_string(),
    };
    state::save_open_session(&mut guard, Some(&session), 61_000).unwrap();
    assert!(guard.get_meta("open_session").unwrap().is_some());

    let recovered = state::recover_open_session(&mut guard).unwrap();
    assert_eq!(
        recovered,
        Some(AppSession {
            ts_start_ms: 1000,
            ts_end_ms: 61_000,
            process_name: "app.exe".to_string(),
            window_title: "win".to_string(),
        })
    );
    // The session row is [ts_start_ms, saved_at_ms]. Count via the same
    // guard — `count_rows` re-locks the store and would deadlock here.
    let sessions: i64 = guard
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM \"app_sessions\"", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sessions, 1);

    // The meta key is cleared; no double recovery.
    assert_eq!(guard.get_meta("open_session").unwrap(), None);
    assert_eq!(state::recover_open_session(&mut guard).unwrap(), None);
}

#[test]
fn counters_persist_and_reload() {
    let (_dir, db) = store();
    // Absent keys on a fresh store must (F4/F6) be tolerated: every key keeps
    // its seed value.
    let fresh = Arc::new(Counters::default());
    {
        let guard = db.lock().unwrap();
        state::load_counters(&guard, &fresh).unwrap();
    }
    assert_eq!(counter(&fresh, "udp_capped"), 0);

    let original = Arc::new(Counters::default());
    original.add("udp_capped", 7);
    original.add("etw_events_lost", 3);
    {
        let mut guard = db.lock().unwrap();
        state::persist_counters(&mut guard, &original).unwrap();
    }

    let reloaded = Arc::new(Counters::default());
    {
        let guard = db.lock().unwrap();
        state::load_counters(&guard, &reloaded).unwrap();
    }
    assert_eq!(counter(&reloaded, "udp_capped"), 7);
    assert_eq!(counter(&reloaded, "etw_events_lost"), 3);
}

#[test]
fn due_logic() {
    assert!(due(10, None, 5));
    assert!(!due(10, Some(8), 5));
    assert!(due(13, Some(8), 5));
}
