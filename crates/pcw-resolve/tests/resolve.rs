//! PTR fallback worker tests (plan Task 8): the resolver never touches the
//! store lock during a lookup, failed lookups feed a 24 h negative cache,
//! DNS-named rows are untouchable, and unnameable IPs cannot starve new ones.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pcw_resolve::{NameResolver, resolve_pending};
use pcw_storage::{SharedStore, Store, WriteOp};
use pcw_types::{ConnectionEvent, Direction, DnsAnswer, EventKind, Protocol};

const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;

/// Fixed epoch timestamp so no test depends on wall-clock time.
const NOW: u64 = 1_700_000_000_000;

type Row = (Option<String>, String);

fn row(store: &SharedStore, ip: &str) -> Row {
    let mut s = store.lock().unwrap();
    s.conn_for_tests()
        .query_row(
            "SELECT hostname, source FROM hostnames WHERE ip = ?1",
            [ip],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

/// Opens a temp-FILE database (WAL side effects are real) behind the shared
/// handle the worker consumes.
fn shared() -> (tempfile::TempDir, SharedStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data.db")).unwrap();
    (dir, Arc::new(Mutex::new(store)))
}

/// One UDP datagram from `ip`: each is its own row, and the first-seen IP
/// gets a pending hostname row the worker picks up.
fn udp(ip: &str) -> WriteOp {
    WriteOp::Connection(ConnectionEvent {
        ts_ms: NOW,
        kind: EventKind::Packet,
        pid: 500,
        process_name: "seed.exe".to_string(),
        process_path: r"C:\tests\seed.exe".to_string(),
        remote_ip: ip.to_string(),
        remote_port: 53,
        local_port: 55_000,
        protocol: Protocol::Udp,
        direction: Direction::Out,
    })
}

fn seed_pending(store: &SharedStore, ips: &[&str]) {
    let ops: Vec<WriteOp> = ips.iter().map(|&ip| udp(ip)).collect();
    store.lock().unwrap().apply_batch(&ops, NOW).unwrap();
}

#[derive(Default)]
struct State {
    calls: Vec<String>,
    /// How many lookups saw the store mutex free while they ran.
    lock_free: usize,
}

/// Tracking fake: answers from a fixed map (unlisted IPs fail), counts every
/// call, and records that the store lock was free during each lookup.
struct Fake {
    db: SharedStore,
    responses: HashMap<String, Option<String>>,
    state: Mutex<State>,
}

impl Fake {
    fn map(db: &SharedStore, entries: &[(&str, Option<&str>)]) -> Self {
        Self {
            db: Arc::clone(db),
            responses: entries
                .iter()
                .map(|(ip, name)| (ip.to_string(), name.map(str::to_string)))
                .collect(),
            state: Mutex::default(),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    /// Every lookup ran without forcing the store lock to fail.
    fn looked_up_unlocked(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.lock_free == st.calls.len()
    }
}

impl NameResolver for Fake {
    fn lookup(&self, ip: &str) -> Option<String> {
        // The worker must not hold the store lock while a DNS call runs.
        if self.db.try_lock().is_ok() {
            self.state.lock().unwrap().lock_free += 1;
        }
        self.state.lock().unwrap().calls.push(ip.to_string());
        self.responses.get(ip).cloned().flatten()
    }
}

#[test]
fn resolved_names_are_stored_as_ptr() {
    let (_dir, db) = shared();
    seed_pending(&db, &["1.1.1.1"]);
    let fake = Fake::map(&db, &[("1.1.1.1", Some("one.one.one.one"))]);

    let named = resolve_pending(&db, &fake, 10, NOW).unwrap();

    assert_eq!(named, 1);
    assert_eq!(fake.calls(), vec!["1.1.1.1".to_string()]);
    let (hostname, source) = row(&db, "1.1.1.1");
    assert_eq!(hostname.as_deref(), Some("one.one.one.one"));
    assert_eq!(source, "ptr");
    assert!(fake.looked_up_unlocked());
}

#[test]
fn failed_lookup_is_not_retried_within_24h_but_is_after() {
    let (_dir, db) = shared();
    seed_pending(&db, &["9.9.9.9"]);
    let fake = Fake::map(&db, &[("9.9.9.9", None)]);

    // First run: the lookup fails, the failure is recorded as source 'none'.
    assert_eq!(resolve_pending(&db, &fake, 10, NOW).unwrap(), 0);
    assert_eq!(fake.calls(), vec!["9.9.9.9".to_string()]);
    assert_eq!(row(&db, "9.9.9.9"), (None, "none".to_string()));

    // One hour later the failure is still too fresh: no retry happens.
    assert_eq!(resolve_pending(&db, &fake, 10, NOW + HOUR_MS).unwrap(), 0);
    assert_eq!(fake.calls().len(), 1);

    // After 24 h the IP is retried exactly once more.
    assert_eq!(resolve_pending(&db, &fake, 10, NOW + DAY_MS).unwrap(), 0);
    assert_eq!(fake.calls().len(), 2);
    assert!(fake.looked_up_unlocked());
}

#[test]
fn dns_names_are_never_overwritten_by_ptr() {
    let (_dir, db) = shared();
    seed_pending(&db, &["8.8.8.8"]);
    db.lock()
        .unwrap()
        .apply_batch(
            &[WriteOp::Dns(DnsAnswer {
                ts_ms: NOW,
                name: "dns.example.test".to_string(),
                ips: vec!["8.8.8.8".to_string()],
            })],
            NOW,
        )
        .unwrap();
    let fake = Fake::map(&db, &[("8.8.8.8", Some("ptr.example.test"))]);

    // The DNS-named row is not pending, so the worker never even sees it.
    let named = resolve_pending(&db, &fake, 10, NOW + DAY_MS).unwrap();

    assert_eq!(named, 0);
    assert!(fake.calls().is_empty());
    let (hostname, source) = row(&db, "8.8.8.8");
    assert_eq!(hostname.as_deref(), Some("dns.example.test"));
    assert_eq!(source, "dns");

    // Belt and braces on the storage invariant: even a direct PTR write
    // cannot touch a 'dns' row.
    db.lock()
        .unwrap()
        .set_ptr_result("8.8.8.8", Some("clobber.example.test"), NOW + DAY_MS)
        .unwrap();
    let (hostname, source) = row(&db, "8.8.8.8");
    assert_eq!(hostname.as_deref(), Some("dns.example.test"));
    assert_eq!(source, "dns");
}

#[test]
fn unnameable_ips_do_not_starve_new_ones() {
    const FAILING: usize = 200;
    const LIMIT: u32 = 50;
    const ROUNDS: usize = 5;

    let (_dir, db) = shared();
    // 200 IPs the fake cannot name, plus one it names, all pending at once.
    let nameable = "203.0.113.7".to_string();
    let mut ips: Vec<String> = (0..FAILING).map(|i| format!("198.51.0.{i}")).collect();
    ips.push(nameable.clone());

    seed_pending(&db, &ips.iter().map(String::as_str).collect::<Vec<_>>());
    let fake = Fake::map(&db, &[(nameable.as_str(), Some("good.example.test"))]);

    // Each round consumes up to LIMIT fresh pending IPs; failed ones become
    // recent 'none' rows and drop out of the queue for 24 h, so every IP is
    // reached within LIMIT-sized rounds instead of being starved forever.
    for _ in 0..ROUNDS {
        resolve_pending(&db, &fake, LIMIT, NOW).unwrap();
    }

    // Every IP was tried exactly once, including the nameable one.
    let mut expected = ips.clone();
    expected.sort();
    let mut called = fake.calls();
    called.sort();
    called.dedup();
    assert_eq!(called.len(), expected.len());
    assert_eq!(called, expected);

    // The nameable one is now a resolved 'ptr' row.
    let (hostname, source) = row(&db, &nameable);
    assert_eq!(hostname.as_deref(), Some("good.example.test"));
    assert_eq!(source, "ptr");
    assert!(fake.looked_up_unlocked());
}
