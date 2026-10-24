//! Agent-core tests (plan Task 12): writer flush cadence, low-disk UDP
//! dropping, disk-full survival, crash recovery, counter persistence,
//! timer logic.
//!
//! All stores are seeded from temp-FILE databases (F8): `Store::open` asserts
//! WAL mode, which `:memory:` cannot provide.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pcw_agent::state;
use pcw_agent::timers::due;
use pcw_agent::tracker::tracker_loop;
use pcw_agent::writer::{WriterConfig, WriterMsg, run_writer};
use pcw_foreground::{OpenSession, SessionSignal};
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

/// The lifetime supervisor counters must survive a reload: the first timer
/// tick publishes the supervisor's totals with `.set`, so a FRESH supervisor
/// would zero the loaded `watchdog_restarts`/`etw_events_lost`/
/// `etw_buffers_lost` — `resume_supervisor` seeds them instead. This test
/// replays run_agent + the tick publish block (timer_tick lines ~500-511)
/// and asserts the published values never drop below the loaded ones.
#[test]
fn supervisor_counters_survive_reload_and_tick_publish() {
    let (_dir, db) = store();
    // A previous run persisted nonzero lifetime totals.
    let previous = Arc::new(Counters::default());
    previous.set("watchdog_restarts", 5);
    previous.set("etw_events_lost", 11);
    previous.set("etw_buffers_lost", 4);
    {
        let mut guard = db.lock().unwrap();
        state::persist_counters(&mut guard, &previous).unwrap();
    }

    // Fresh run: load the counters and build the supervisor exactly the way
    // run_agent does.
    let reloaded = Arc::new(Counters::default());
    {
        let guard = db.lock().unwrap();
        state::load_counters(&guard, &reloaded).unwrap();
    }
    let mut supervisor = state::resume_supervisor(&reloaded);
    assert_eq!(supervisor.restarts, 5);
    assert_eq!(supervisor.events_lost, 11);
    assert_eq!(supervisor.buffers_lost, 4);

    // Timer tick: the live session reports loss, the session ends, and the
    // tick publishes the (cumulative) supervisor values with `.set`.
    supervisor.on_stats(19, 7);
    let _ = supervisor.on_exit(Ok(()), Duration::from_secs(60));
    reloaded.set("etw_events_lost", supervisor.events_lost);
    reloaded.set("etw_buffers_lost", supervisor.buffers_lost);
    reloaded.set("watchdog_restarts", supervisor.restarts);

    // The loaded lifetime values are not zeroed (they only grow).
    assert_eq!(counter(&reloaded, "watchdog_restarts"), 6);
    assert_eq!(counter(&reloaded, "etw_events_lost"), 30);
    assert_eq!(counter(&reloaded, "etw_buffers_lost"), 11);
}

#[test]
fn due_logic() {
    assert!(due(10, None, 5));
    assert!(!due(10, Some(8), 5));
    assert!(due(13, Some(8), 5));
}

// ---- tracker stop path --------------------------------------------------

/// Drains the writer channel into a plain Vec (the tracker has exited, so
/// `try_recv` Empty = drained).
fn drain_writer(rx: Receiver<WriterMsg>) -> Vec<WriterMsg> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// Watches a thread on a channel so `wait_until` can detect it RETURNING
/// (a `JoinHandle` cannot be polled with a timeout directly). In the old
/// sender-disconnect-only design this signal never came when a sender clone
/// was held by a menu closure, and the assertion below fired.
///
/// Call rules: a tiny heartbeat lets test 1 OBSERVE when a signal has been
/// consumed (its heartbeat `OpenSession` appears); a huge one (test 2)
/// suppresses heartbeats so every `OpenSession` message in the output belongs
/// to the stop sequence itself.
fn spawn_tracker(
    rx: Receiver<SessionSignal>,
    tx_writer: mpsc::Sender<WriterMsg>,
    heartbeat: Duration,
    stop: Arc<AtomicBool>,
) -> (mpsc::Receiver<()>, std::thread::JoinHandle<()>) {
    let (done_tx, done_rx) = channel();
    let handle = std::thread::spawn(move || {
        tracker_loop(rx, tx_writer, false, heartbeat, stop);
        let _ = done_tx.send(());
    });
    (done_rx, handle)
}

/// The deterministic stop signal (review finding 1): raising the flag makes
/// the tracker finalize EXACTLY ONCE, post the final outputs +
/// `OpenSession {None}` + `Shutdown` — and, crucially, RETURN, where the old
/// sender-disconnect-only design hung forever because the tray's pause-menu
/// closure kept a sender clone alive.
#[test]
fn tracker_stop_flag_finalizes_once_posts_none_and_shutdown_and_returns() {
    let (tx_sig, rx_sig) = channel();
    let (tx_w, rx_w) = channel::<WriterMsg>();
    let stop = Arc::new(AtomicBool::new(false));
    let (done, handle) = spawn_tracker(rx_sig, tx_w, Duration::from_millis(30), Arc::clone(&stop));

    // One foreground signal: finalize must close exactly this one session.
    tx_sig
        .send(SessionSignal::Foreground {
            ts_ms: 1000,
            process_name: "app.exe".to_string(),
            window_title: "Session Top".to_string(),
        })
        .unwrap();

    // Emulate production: a sender clone captured by the tray pause-menu
    // closure and held for the agent's lifetime — sender-disconnect can
    // NEVER fire, so only the stop flag can end the loop.
    let held_by_supposed_menu_closure = {
        let tx = tx_sig.clone();
        Some(Box::new(move || {
            let _ = tx.send(SessionSignal::Paused { ts_ms: 99_999 });
            true
        }))
    };
    // Never dropped, like main's menu handler: `forget` keeps the leak
    // contained to this test (the sender clone stays functional, not UB).
    std::mem::forget(held_by_supposed_menu_closure);

    // Everything the channel has produced, kept for the final accounting
    // (`Receiver` is not `Clone`, so the wait below must be non-destructive).
    let seen: Arc<Mutex<Vec<WriterMsg>>> = Arc::new(Mutex::new(Vec::new()));

    // The tracker's heartbeat `OpenSession` proves the signal was consumed
    // BEFORE the stop flag is raised (message order is preserved).
    assert!(
        wait_until(Duration::from_secs(2), || {
            let mut cache = seen.lock().unwrap();
            while let Ok(m) = rx_w.try_recv() {
                cache.push(m);
            }
            cache.iter().any(|m| {
                matches!(
                    m,
                    WriterMsg::OpenSession { session: Some(s), .. }
                        if s.process_name == "app.exe"
                )
            })
        }),
        "no heartbeat OpenSession for app.exe: the signal was not consumed"
    );

    stop.store(true, Ordering::Relaxed);
    drop(tx_sig); // both triggers at once: finalize must still run once
    // Old design: the tracker looped on Timeout forever and this timed out.
    assert!(
        wait_until(Duration::from_secs(5), || done.try_recv().is_ok()),
        "tracker did not RETURN after the stop signal (old disconnect-only design hangs)"
    );
    handle.join().unwrap();
    while let Ok(m) = rx_w.try_recv() {
        seen.lock().unwrap().push(m);
    }
    // `WriterMsg` is not `Clone`: take the accumulated messages by value.
    let messages = std::mem::take(&mut *seen.lock().unwrap());
    let mut sessions = 0;
    let mut titles = 0;
    let mut opens_none = 0;
    let mut opens_some = 0;
    let mut shutdowns = 0;
    for msg in messages {
        match msg {
            WriterMsg::Op(WriteOp::Session(_)) => sessions += 1,
            WriterMsg::Op(WriteOp::Title(_)) => titles += 1,
            WriterMsg::Op(_) => {}
            WriterMsg::OpenSession { session: None, .. } => opens_none += 1,
            WriterMsg::OpenSession {
                session: Some(_), ..
            } => opens_some += 1,
            WriterMsg::Shutdown => shutdowns += 1,
        }
    }
    // Finalize exactly once ⇒ exactly one closed session.
    assert_eq!(sessions, 1, "finalize must emit exactly one session");
    assert_eq!(titles, 0, "no title was shown long enough to be logged");
    assert_eq!(opens_none, 1, "exactly one final OpenSession {{None}}");
    assert!(
        opens_some >= 1,
        "the observed heartbeat OpenSession vanished"
    );
    assert_eq!(shutdowns, 1, "exactly one WriterMsg::Shutdown");
}

/// The preserved secondary trigger: with NO stop flag, all signal senders
/// dropped still lets the tracker finalize (once) and RETURN (tracker
/// panic-safety, e.g. when main itself is gone).
#[test]
fn tracker_sender_disconnect_still_finalizes_and_returns() {
    let (tx_sig, rx_sig) = channel();
    let (tx_w, rx_w) = channel::<WriterMsg>();
    let stop = Arc::new(AtomicBool::new(false));
    let (done, handle) = spawn_tracker(rx_sig, tx_w, Duration::from_secs(3_600), stop);

    drop(tx_sig);
    assert!(
        wait_until(Duration::from_secs(5), || done.try_recv().is_ok()),
        "tracker did not return after all senders were dropped"
    );
    handle.join().unwrap();

    let messages = drain_writer(rx_w);
    let mut opens_none = 0;
    let mut shutdowns = 0;
    let mut other = 0;
    for msg in messages {
        match msg {
            WriterMsg::OpenSession { session: None, .. } => opens_none += 1,
            WriterMsg::Shutdown => shutdowns += 1,
            _ => other += 1,
        }
    }
    assert_eq!(opens_none, 1);
    assert_eq!(shutdowns, 1);
    assert_eq!(other, 0, "no tracker outputs without any session to close");
}
