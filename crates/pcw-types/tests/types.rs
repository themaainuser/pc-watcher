use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

use pcw_types::{
    ConnectionEvent, Counters, Direction, EventKind, ProcessCache, Protocol, file_name_of,
};

#[test]
fn connection_event_roundtrips_json() {
    let ev = ConnectionEvent {
        ts_ms: 123,
        kind: EventKind::Open,
        pid: 42,
        process_name: "x.exe".into(),
        process_path: "C:\\x.exe".into(),
        remote_ip: "1.2.3.4".into(),
        remote_port: 443,
        local_port: 51000,
        protocol: Protocol::Tcp,
        direction: Direction::Out,
    };
    let json = serde_json::to_string(&ev).unwrap();
    let back: ConnectionEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(back, ev);
}

#[test]
fn protocol_and_direction_text_roundtrip() {
    assert_eq!(Protocol::parse(Protocol::Udp.as_str()), Some(Protocol::Udp));
    assert_eq!(Direction::In.as_str(), "in");
    assert_eq!(Protocol::parse("xyz"), None);
}

#[test]
fn process_cache_queries_once_then_caches() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = calls.clone();
    let mut c = ProcessCache::new(Box::new(move |_| {
        c2.fetch_add(1, SeqCst);
        Some(("x.exe".into(), "C:\\x.exe".into()))
    }));
    c.get(7);
    c.get(7);
    assert_eq!(calls.load(SeqCst), 1);
}

#[test]
fn failed_lookup_is_unknown_and_retried() {
    let fail = Arc::new(AtomicBool::new(true));
    let f2 = fail.clone(); // shared flag: a `move`d bool would be a copy
    let mut c = ProcessCache::new(Box::new(move |_| {
        if f2.load(SeqCst) {
            None
        } else {
            Some(("y.exe".into(), String::new()))
        }
    }));
    assert_eq!(c.get(9).0, "unknown");
    assert_eq!(c.get(9).0, "unknown"); // failure not cached: still queried after flag flips
    fail.store(false, SeqCst);
    assert_eq!(c.get(9).0, "y.exe");
}

#[test]
fn process_start_invalidates_reused_pid() {
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let mut c = ProcessCache::new(Box::new(move |_| {
        let i = n2.fetch_add(1, SeqCst);
        if i == 0 {
            Some(("a.exe".into(), "C:\\a.exe".into()))
        } else {
            Some(("b.exe".into(), "C:\\b.exe".into()))
        }
    }));
    assert_eq!(c.get(5).0, "a.exe");
    c.note_process(5, "C:\\b.exe", true);
    assert_eq!(c.get(5).0, "b.exe");
}

#[test]
fn exited_process_falls_back_to_event_image() {
    let mut c = ProcessCache::new(Box::new(|_| None));
    c.note_process(6, "\\Device\\HarddiskVolume3\\App\\z.exe", false);
    let (name, path) = c.get(6);
    assert_eq!(name, "z.exe");
    assert_eq!(path, "\\Device\\HarddiskVolume3\\App\\z.exe");
}

#[test]
fn file_name_of_handles_dos_and_device_paths() {
    assert_eq!(file_name_of("C:\\a\\b.exe"), "b.exe");
    assert_eq!(file_name_of("\\Device\\HarddiskVolume3\\x\\y.exe"), "y.exe");
    assert_eq!(file_name_of("no-slash"), "no-slash");
}

#[test]
fn counters_snapshot_lists_all_six_keys_in_order() {
    let counters = Counters::default();
    counters.add("udp_capped", 3);
    counters.set("watchdog_restarts", 2);
    let snap = counters.snapshot();
    assert_eq!(snap.len(), 6);
    assert_eq!(snap[3], ("udp_capped", 3));
    assert_eq!(snap[2], ("watchdog_restarts", 2));
    assert_eq!(snap[0], ("etw_events_lost", 0));
}
