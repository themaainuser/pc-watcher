//! Read-path tests (plan Task 5): query filters, hostname worker support,
//! streaming export readers and WAL read/write concurrency.

use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use pcw_storage::{ConnFilter, Store, WriteOp};
use pcw_types::{
    AppSession, ConnectionEvent, Direction, DnsAnswer, EventKind, Protocol, WindowTitle,
};

const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// SQLite-local hour (0..23) of `ts_ms`, computed through the same
/// `strftime('…', 'localtime')` expression the overview query uses, so the
/// seed stays consistent with the bucketing under test on any machine.
fn local_hour(store: &mut Store, ts_ms: u64) -> u32 {
    let hour: String = store
        .conn_for_tests()
        .query_row(
            "SELECT strftime('%H', ?1 / 1000, 'unixepoch', 'localtime')",
            [ts_ms as i64],
            |r| r.get(0),
        )
        .unwrap();
    hour.parse().unwrap()
}

/// Top-of-hour timestamp whose SQLite-local hour is `target`, searched
/// backwards over two days so DST shifts cannot break the seed.
fn ts_at_local_hour(store: &mut Store, target: u32) -> u64 {
    let base = now_ms() / HOUR_MS * HOUR_MS;
    for back in 0..48u64 {
        let ts = base - back * HOUR_MS;
        if local_hour(store, ts) == target {
            return ts;
        }
    }
    panic!("no local hour {target} found within 48 h backwards");
}

/// First moment of local `hour:minute`. Starts from `ts_at_local_hour` and
/// corrects the remaining minutes (an epoch-hour boundary is not a local-hour
/// boundary in non-UTC-offset zones), re-checking until SQLite agrees.
fn ts_at_local_time(store: &mut Store, hour: u32, minute: u32) -> u64 {
    let mut ts = ts_at_local_hour(store, hour);
    for _ in 0..4 {
        let (h, m): (String, String) = store
            .conn_for_tests()
            .query_row(
                "SELECT strftime('%H', ?1 / 1000, 'unixepoch', 'localtime'),
                        strftime('%M', ?1 / 1000, 'unixepoch', 'localtime')",
                [ts as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let (h, m): (u32, u32) = (h.parse().unwrap(), m.parse().unwrap());
        if (h, m) == (hour, minute) {
            return ts;
        }
        let ts_i = i64::try_from(ts).unwrap_or(0);
        ts = (ts_i - (i64::from(m) - i64::from(minute)) * 60_000) as u64;
    }
    panic!("could not resolve local {hour:02}:{minute:02}");
}

#[derive(Debug, Clone, Copy)]
struct Seed {
    /// Top of local hour 9.
    t9: u64,
    /// Top of local hour 10 (after `t9`, same day).
    t10: u64,
}

fn tcp(
    ts: u64,
    name: &str,
    pid: u32,
    ip: &str,
    remote: u16,
    local: u16,
    dir: Direction,
) -> WriteOp {
    WriteOp::Connection(ConnectionEvent {
        ts_ms: ts,
        // Standalone close: one row per event, without an open-map entry.
        kind: EventKind::Close,
        pid,
        process_name: name.to_string(),
        process_path: format!(r"C:\tests\{name}"),
        remote_ip: ip.to_string(),
        remote_port: remote,
        local_port: local,
        protocol: Protocol::Tcp,
        direction: dir,
    })
}

fn udp(
    ts: u64,
    name: &str,
    pid: u32,
    ip: &str,
    remote: u16,
    local: u16,
    dir: Direction,
) -> WriteOp {
    WriteOp::Connection(ConnectionEvent {
        ts_ms: ts,
        kind: EventKind::Packet,
        pid,
        process_name: name.to_string(),
        process_path: format!(r"C:\tests\{name}"),
        remote_ip: ip.to_string(),
        remote_port: remote,
        local_port: local,
        protocol: Protocol::Udp,
        direction: dir,
    })
}

/// Seeds the fixed fixture from the plan brief on a fresh temp-FILE database:
///
/// apps `a.exe` (3 TCP + 2 UDP) / `b.exe` (1 TCP), IPs 1.1.1.1 / 2.2.2.2,
/// ports 443/53, timestamps at local hours 9 and 10, two sessions and three
/// titles. Connection rows are inserted in this order, so their ids are:
///
/// | id | app   | ip      | port | proto | dir | ts          |
/// |----|-------|---------|------|-------|-----|-------------|
/// | 1  | a.exe | 1.1.1.1 | 443  | tcp   | out | t9 + 10 min |
/// | 2  | a.exe | 1.1.1.1 | 53   | tcp   | out | t9 + 20 min |
/// | 3  | a.exe | 2.2.2.2 | 443  | tcp   | in  | t10 + 5 min |
/// | 4  | a.exe | 2.2.2.2 | 53   | udp   | out | t9 + 30 min |
/// | 5  | a.exe | 2.2.2.2 | 53   | udp   | in  | t9 + 31 min |
/// | 6  | b.exe | 2.2.2.2 | 443  | tcp   | out | t9 + 15 min |
///
/// A DNS answer afterwards names 1.1.1.1 as `one.one.one.one` (upgrading its
/// pending hostnames row to source 'dns'); 2.2.2.2 stays pending.
fn seed() -> (tempfile::TempDir, Store, Seed) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let mut store = Store::open(&db).unwrap();

    let t9 = ts_at_local_time(&mut store, 9, 0);
    let mut t10 = t9 + HOUR_MS;
    // One step normally; extra steps only survive a DST shift between 9 and 10.
    while local_hour(&mut store, t10) != 10 {
        t10 += HOUR_MS;
    }
    let seed = Seed { t9, t10 };

    let ops = vec![
        tcp(
            t9 + 600_000,
            "a.exe",
            100,
            "1.1.1.1",
            443,
            55001,
            Direction::Out,
        ),
        tcp(
            t9 + 1_200_000,
            "a.exe",
            100,
            "1.1.1.1",
            53,
            55002,
            Direction::Out,
        ),
        tcp(
            t10 + 300_000,
            "a.exe",
            100,
            "2.2.2.2",
            443,
            55003,
            Direction::In,
        ),
        udp(
            t9 + 1_800_000,
            "a.exe",
            100,
            "2.2.2.2",
            53,
            55004,
            Direction::Out,
        ),
        udp(
            t9 + 1_860_000,
            "a.exe",
            100,
            "2.2.2.2",
            53,
            55005,
            Direction::In,
        ),
        tcp(
            t9 + 900_000,
            "b.exe",
            200,
            "2.2.2.2",
            443,
            55006,
            Direction::Out,
        ),
        WriteOp::Session(AppSession {
            ts_start_ms: t9,
            ts_end_ms: t9 + 300_000, // 300 s
            process_name: "a.exe".to_string(),
            window_title: "session one".to_string(),
        }),
        WriteOp::Session(AppSession {
            ts_start_ms: t9 + 600_000,
            ts_end_ms: t9 + 1_000_000, // 400 s
            process_name: "a.exe".to_string(),
            window_title: "session two".to_string(),
        }),
        WriteOp::Title(WindowTitle {
            ts_ms: t9 + 100_000,
            process_name: "a.exe".to_string(),
            title: "alpha".to_string(),
        }),
        WriteOp::Title(WindowTitle {
            ts_ms: t9 + 200_000,
            process_name: "a.exe".to_string(),
            title: "bravo".to_string(),
        }),
        WriteOp::Title(WindowTitle {
            ts_ms: t10 + 100_000,
            process_name: "a.exe".to_string(),
            title: "charlie".to_string(),
        }),
        // Names 1.1.1.1, whose pending hostnames row the TCP events created.
        WriteOp::Dns(DnsAnswer {
            ts_ms: t9,
            name: "one.one.one.one".to_string(),
            ips: vec!["1.1.1.1".to_string()],
        }),
    ];
    let now = t10 + DAY_MS; // after every seeded event
    store.apply_batch(&ops, now).unwrap();
    (dir, store, seed)
}

#[test]
fn overview_counts_orders_and_buckets_hours() {
    let (_dir, store, seed) = seed();

    let ov = store.overview(&ConnFilter::default()).unwrap();
    assert_eq!(ov.total_connections, 6);
    assert_eq!(
        ov.top_apps,
        vec![("a.exe".to_string(), 5), ("b.exe".to_string(), 1)]
    );
    // 1.1.1.1 known by hostname (rows 1 and 2); 2.2.2.2 unknown → falls back
    // to its IP (rows 3–6).
    assert_eq!(
        ov.top_services,
        vec![
            ("2.2.2.2".to_string(), 4),
            ("one.one.one.one".to_string(), 2)
        ]
    );
    assert_eq!(ov.hourly, vec![(9, 5), (10, 1)]);

    // A filter that excludes everything: rows only exist in hours 9/10.
    let low = seed.t10 + DAY_MS;
    let f = ConnFilter {
        from_ms: low,
        to_ms: low + DAY_MS,
        ..Default::default()
    };
    let ov = store.overview(&f).unwrap();
    assert_eq!(ov.total_connections, 0);
    assert!(ov.top_apps.is_empty() && ov.top_services.is_empty() && ov.hourly.is_empty());
}

#[test]
fn filters_by_protocol_port_direction() {
    let (_dir, store, _s) = seed();
    let ids = |f: &ConnFilter| -> Vec<i64> {
        store
            .connections(f, 100)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect()
    };

    // Udp → rows 4 and 5, newest (highest ts/id) first.
    let f = ConnFilter {
        protocol: Some(Protocol::Udp),
        ..Default::default()
    };
    assert_eq!(ids(&f), vec![5, 4]);

    // Port filter matches remote ports (rows 2/4/5 have remote port 53)…
    let f = ConnFilter {
        port: Some(53),
        ..Default::default()
    };
    assert_eq!(ids(&f), vec![5, 4, 2]);

    // …and local ports (row 6 has local port 55006).
    let f = ConnFilter {
        port: Some(55006),
        ..Default::default()
    };
    assert_eq!(ids(&f), vec![6]);

    // Direction In → rows 3 and 5, newest first (row 3 sits in hour 10).
    let f = ConnFilter {
        direction: Some(Direction::In),
        ..Default::default()
    };
    assert_eq!(ids(&f), vec![3, 5]);

    // App filter, newest first.
    let f = ConnFilter {
        app: Some("b.exe".to_string()),
        ..Default::default()
    };
    assert_eq!(ids(&f), vec![6]);

    // A disjoint window excludes everything.
    let f = ConnFilter {
        from_ms: u64::MAX - 1,
        ..Default::default()
    };
    assert!(ids(&f).is_empty());
}

#[test]
fn app_destinations_group_by_tuple() {
    let (_dir, store, seed) = seed();

    let f = ConnFilter {
        app: Some("a.exe".to_string()),
        ..Default::default()
    };
    let mut rows = store.app_destinations(&f).unwrap();
    // Deterministic order: by count desc, then ip/port/protocol.
    rows.sort_by(|a, b| {
        (a.remote_ip.clone(), a.remote_port, a.protocol.as_str()).cmp(&(
            b.remote_ip.clone(),
            b.remote_port,
            b.protocol.as_str(),
        ))
    });

    let key = |r: &pcw_storage::DestinationRow| (r.remote_ip.clone(), r.remote_port, r.protocol);
    let mut it = rows.iter();
    let r = it.next().unwrap();
    assert_eq!(key(r), ("1.1.1.1".to_string(), 53, Protocol::Tcp));
    assert_eq!(r.hostname.as_deref(), Some("one.one.one.one"));
    assert_eq!(r.count, 1);
    assert_eq!(r.ts_first_ms, seed.t9 + 1_200_000);
    assert_eq!(r.ts_last_ms, seed.t9 + 1_200_000);
    let r = it.next().unwrap();
    assert_eq!(key(r), ("1.1.1.1".to_string(), 443, Protocol::Tcp));
    assert_eq!(r.hostname.as_deref(), Some("one.one.one.one"));
    assert_eq!(r.count, 1);
    assert_eq!(r.ts_first_ms, seed.t9 + 600_000);
    let r = it.next().unwrap();
    assert_eq!(key(r), ("2.2.2.2".to_string(), 53, Protocol::Udp));
    assert_eq!(r.hostname, None);
    assert_eq!(r.count, 2);
    assert_eq!(r.ts_first_ms, seed.t9 + 1_800_000);
    assert_eq!(r.ts_last_ms, seed.t9 + 1_860_000);
    let r = it.next().unwrap();
    assert_eq!(key(r), ("2.2.2.2".to_string(), 443, Protocol::Tcp));
    assert_eq!(r.hostname, None);
    assert_eq!(r.count, 1);
    assert_eq!(r.ts_first_ms, seed.t10 + 300_000);
    assert!(it.next().is_none());
}

#[test]
fn service_connections_match_hostname_or_ip() {
    let (_dir, store, _s) = seed();

    // Same rows whether the caller passes the hostname or the bare IP.
    let by_name = store
        .service_connections("one.one.one.one", &ConnFilter::default(), 100)
        .unwrap();
    let by_ip = store
        .service_connections("1.1.1.1", &ConnFilter::default(), 100)
        .unwrap();
    assert_eq!(
        by_name.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![2, 1],
        "newest first, only the 1.1.1.1 rows"
    );
    assert_eq!(
        by_name
            .iter()
            .map(|r| (r.id, r.hostname.clone()))
            .collect::<Vec<_>>(),
        by_ip
            .iter()
            .map(|r| (r.id, r.hostname.clone()))
            .collect::<Vec<_>>()
    );
    assert!(
        by_name
            .iter()
            .all(|r| r.hostname.as_deref() == Some("one.one.one.one"))
    );

    // Unknown service → nothing.
    assert!(
        store
            .service_connections("nope.example.test", &ConnFilter::default(), 100)
            .unwrap()
            .is_empty()
    );

    // The limit applies.
    assert_eq!(
        store
            .service_connections("1.1.1.1", &ConnFilter::default(), 1)
            .unwrap()
            .len(),
        1
    );
    // Filters still apply on top of the service key (both 1.1.1.1 rows are
    // TCP, so a UDP filter leaves nothing).
    let f = ConnFilter {
        protocol: Some(Protocol::Udp),
        ..Default::default()
    };
    assert!(
        store
            .service_connections("1.1.1.1", &f, 100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn usage_summary_and_titles_for_app() {
    let (_dir, store, seed) = seed();

    let usage = store.usage_summary("a.exe", 0, u64::MAX).unwrap();
    assert_eq!(usage.session_count, 2);
    assert_eq!(usage.total_secs, 700); // 300 s + 400 s

    // Unknown app: zero summary.
    let none = store.usage_summary("b.exe", 0, u64::MAX).unwrap();
    assert_eq!((none.session_count, none.total_secs), (0, 0));

    // Titles for a.exe in time order.
    let titles = store
        .titles(0, u64::MAX, Some("a.exe"))
        .unwrap()
        .into_iter()
        .map(|t| t.title)
        .collect::<Vec<_>>();
    assert_eq!(titles, vec!["alpha", "bravo", "charlie"]);

    // The app filter excludes others; windows are inclusive/exclusive bounds.
    let none = store.titles(0, u64::MAX, Some("b.exe")).unwrap();
    assert!(none.is_empty());
    let bounded = store
        .titles(seed.t9 + 150_000, seed.t10 + 100_000, Some("a.exe"))
        .unwrap();
    assert_eq!(bounded.len(), 1);
    assert_eq!(bounded[0].title, "bravo");

    // Sessions for b.exe: seeded sessions are all a.exe, so none.
    let none = store.sessions(0, u64::MAX, Some("b.exe")).unwrap();
    assert!(none.is_empty());
    let sessions = store.sessions(0, u64::MAX, Some("a.exe")).unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].window_title, "session one");
}

#[test]
fn pending_hostnames_skip_recent_failures() {
    let (_dir, mut store, seed) = seed();

    // Four fresh IPs join: A stays pending; B becomes `none` (recent); C
    // resolves with `Some` → 'ptr'; D is named via DNS → 'dns'.
    let ips = ["10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4"];
    let at = seed.t10 + HOUR_MS;
    let batch: Vec<WriteOp> = ips
        .iter()
        .map(|&ip| udp(at, "c.exe", 300, ip, 53, 55_100, Direction::Out))
        .collect();
    store.apply_batch(&batch, at).unwrap();
    store.set_ptr_result("10.0.0.2", None, at).unwrap();
    store
        .set_ptr_result("10.0.0.3", Some("c.example.test"), at)
        .unwrap();
    store
        .apply_batch(
            &[WriteOp::Dns(DnsAnswer {
                ts_ms: at,
                name: "d.example.test".to_string(),
                ips: vec!["10.0.0.4".to_string()],
            })],
            at,
        )
        .unwrap();

    // A PTR failure is retried only a day later: the still-pending IPs come
    // first (from the seed: 2.2.2.2, plus the freshly joined IP A); recent
    // 'none'/'ptr'/'dns' rows are absent.
    assert_eq!(
        store.pending_hostnames(10, at).unwrap(),
        vec!["10.0.0.1".to_string(), "2.2.2.2".to_string()]
    );
    // After 24 h the failed IP B is retried, ordered behind the pending
    // ones; 'ptr' and 'dns' rows never appear in either pass.
    let list = store.pending_hostnames(10, at + DAY_MS).unwrap();
    assert_eq!(
        list,
        vec![
            "10.0.0.1".to_string(),
            "2.2.2.2".to_string(),
            "10.0.0.2".to_string()
        ]
    );
    assert!(!list.iter().any(|ip| ip == "10.0.0.3" || ip == "10.0.0.4"));

    // The limit applies to the combined list.
    assert_eq!(
        store.pending_hostnames(1, at + DAY_MS).unwrap(),
        vec!["10.0.0.1".to_string()]
    );

    // set_ptr_result never overrides a row named by DNS.
    store
        .set_ptr_result("10.0.0.4", Some("override.example.test"), at)
        .unwrap();
    let (hostname, source): (Option<String>, String) = store
        .conn_for_tests()
        .query_row(
            "SELECT hostname, source FROM hostnames WHERE ip = '10.0.0.4'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(hostname.as_deref(), Some("d.example.test"));
    assert_eq!(source, "dns");
}

#[test]
fn for_each_connection_after_streams_in_id_order() {
    let (_dir, store, _s) = seed();

    let mut seen = Vec::new();
    let last = store
        .for_each_connection_after(2, &mut |row: &pcw_storage::ConnectionRow| {
            seen.push(row.id);
            Ok(())
        })
        .unwrap();
    // Ids 3.. ascending, in id order (not ts order).
    assert_eq!(seen, vec![3, 4, 5, 6]);
    // Highest id passed to the callback, or `after_id` when nothing follows.
    assert_eq!(last, 6);
    assert_eq!(
        store.for_each_connection_after(6, &mut |_| Ok(())).unwrap(),
        6
    );

    // The hostname join is part of the streamed row: only row 2's IP is
    // named, all later rows fall back to a None hostname.
    let mut hostnames = Vec::new();
    store
        .for_each_connection_after(1, &mut |row: &pcw_storage::ConnectionRow| {
            if row.hostname.is_some() {
                hostnames.push((row.id, row.hostname.clone()));
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(hostnames, vec![(2, Some("one.one.one.one".to_string()))]);

    // Session and title streams behave the same way (ids 2.. and 2..).
    let mut session_ids = Vec::new();
    let last = store
        .for_each_session_after(1, &mut |id: i64, _s: &AppSession| {
            session_ids.push(id);
            Ok(())
        })
        .unwrap();
    assert_eq!(session_ids, vec![2]);
    assert_eq!(last, 2);
    let mut title_ids = Vec::new();
    let last = store
        .for_each_title_after(0, &mut |id: i64, _t: &WindowTitle| {
            title_ids.push(id);
            Ok(())
        })
        .unwrap();
    assert_eq!(title_ids, vec![1, 2, 3]);
    assert_eq!(last, 3);

    // A callback error surfaces as StorageError::Callback.
    let err = store
        .for_each_title_after(0, &mut |_id: i64, _t: &WindowTitle| {
            Err(std::io::Error::other("write failed"))
        })
        .unwrap_err();
    assert!(matches!(err, pcw_storage::StorageError::Callback(_)));
}

#[test]
fn reader_does_not_block_while_writer_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let mut writer = Store::open(&db).unwrap();
    let reader = Store::open_readonly(&db).unwrap();

    // 100 batches at ~1 ms: the writer keeps the WAL moving while the reader
    // must keep serving queries without a single failure or lock-up.
    let handle = thread::spawn(move || {
        for i in 0..100u32 {
            let ts = 1_700_000_000_000 + u64::from(i) * 1000;
            let op = WriteOp::Connection(ConnectionEvent {
                ts_ms: ts,
                kind: EventKind::Packet,
                pid: 400,
                process_name: "writer.exe".to_string(),
                process_path: r"C:\tests\writer.exe".to_string(),
                remote_ip: format!("10.1.{}.{}", i / 200, 1 + (i % 200)),
                remote_port: 443,
                local_port: 55_200,
                protocol: Protocol::Udp,
                direction: Direction::Out,
            });
            writer.apply_batch(&[op], ts).unwrap();
            thread::sleep(std::time::Duration::from_millis(1));
        }
    });

    let mut successes = 0u32;
    let mut errors = Vec::new();
    while !handle.is_finished() {
        match reader.overview(&ConnFilter::default()) {
            Ok(_) => successes += 1,
            Err(e) => errors.push(e),
        }
    }
    handle.join().unwrap();

    assert!(
        successes >= 1,
        "no read succeeded while the writer was active"
    );
    assert!(errors.is_empty(), "read errors during writes: {errors:?}");

    // After the writer is gone, the reader sees every row.
    let ov = reader.overview(&ConnFilter::default()).unwrap();
    assert_eq!(ov.total_connections, 100);
}
