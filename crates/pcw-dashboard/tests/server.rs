//! Server tests (plan Task 14): loopback binding, seeded JSON endpoints,
//! filter parsing, read-only lazy recovery, 404/shutdown behavior and the
//! embedded UI's offline guarantee.
//!
//! All requests go through raw `std::net::TcpStream` HTTP/1.0 calls (no new
//! test dependencies); the DB is always a temp *file* seeded with
//! `Store::open` + a `WriteOp` batch (never `:memory:`, F8).

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pcw_dashboard::routes;
use pcw_dashboard::{DashError, DashServer};
use pcw_storage::{Store, WriteOp};
use pcw_types::{
    AppSession, ConnectionEvent, Direction, DnsAnswer, EventKind, Protocol, WindowTitle,
};

/// The exact 503 body the route table mandates.
const NOT_READY_BODY: &str = r#"{"error": "no data yet — is pcw-agent running?"}"#;

const IDLE: Duration = Duration::from_secs(60);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Binds an ephemeral port and runs the server on a background thread.
fn spawn_run(db_path: PathBuf) -> (u16, Arc<AtomicBool>, mpsc::Receiver<Result<(), DashError>>) {
    let server = DashServer::bind(0).expect("bind to an ephemeral loopback port");
    let port = server.local_addr().port();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(server.run(db_path, thread_stop, IDLE));
    });
    (port, stop, rx)
}

/// One raw HTTP/1.0 request; returns (head, body). The server closes the
/// HTTP/1.0 connection after the body, so `read_to_string` sees EOF.
fn raw_request(port: u16, method: &str, target: &str) -> (String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to dashboard");
    let request =
        format!("{method} {target} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("send request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    (head.to_string(), body.to_string())
}

fn status_code(head: &str) -> u16 {
    head.lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .expect("HTTP/1.x status line")
        .parse()
        .expect("numeric status code")
}

/// Seeds a temp-file DB the way the agent would: two connections, one
/// session, one title and a resolved hostname.
fn seed_db(path: &Path) {
    let mut store = Store::open(path).expect("create and open seed db");
    let now = now_ms();
    let batch = vec![
        WriteOp::Connection(ConnectionEvent {
            ts_ms: now,
            kind: EventKind::Close,
            pid: 1001,
            process_name: "a.exe".into(),
            process_path: r"C:\tests\a.exe".into(),
            remote_ip: "93.184.216.34".into(),
            remote_port: 443,
            local_port: 51000,
            protocol: Protocol::Tcp,
            direction: Direction::Out,
        }),
        WriteOp::Connection(ConnectionEvent {
            ts_ms: now,
            kind: EventKind::Packet,
            pid: 1002,
            process_name: "a.exe".into(),
            process_path: r"C:\tests\a.exe".into(),
            remote_ip: "8.8.8.8".into(),
            remote_port: 53,
            local_port: 51001,
            protocol: Protocol::Udp,
            direction: Direction::Out,
        }),
        WriteOp::Session(AppSession {
            ts_start_ms: now - 60_000,
            ts_end_ms: now,
            process_name: "a.exe".into(),
            window_title: "report.doc — Editor".into(),
        }),
        WriteOp::Title(WindowTitle {
            ts_ms: now - 30_000,
            process_name: "a.exe".into(),
            title: "report.doc — Editor".into(),
        }),
        WriteOp::Dns(DnsAnswer {
            ts_ms: now,
            name: "example.test".into(),
            ips: vec!["93.184.216.34".into()],
        }),
    ];
    store.apply_batch(&batch, now).expect("seed batch");
}

#[test]
fn binds_loopback_only() {
    // Review Focus #3: bind(0) → the bound address must be 127.0.0.1, never
    // 0.0.0.0 or [::].
    let server = DashServer::bind(0).expect("bind to an ephemeral port");
    let addr = server.local_addr();
    assert_eq!(addr.ip(), IpAddr::from([127, 0, 0, 1]));
    assert!(addr.port() > 0);
}

#[test]
fn overview_returns_seeded_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db");
    seed_db(&path);
    let (port, _stop, _done) = spawn_run(path);

    let (head, body) = raw_request(port, "GET", "/api/overview");
    assert_eq!(status_code(&head), 200);
    assert!(
        head.to_ascii_lowercase().contains("application/json"),
        "content-type must be application/json, head: {head}"
    );
    assert!(body.contains("a.exe"), "body should name the app: {body}");
    assert!(
        body.contains("example.test"),
        "resolved hostname should surface as a service: {body}"
    );
}

#[test]
fn app_endpoint_has_drill_down_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db");
    seed_db(&path);
    let (port, _stop, _done) = spawn_run(path);

    let (head, body) = raw_request(port, "GET", "/api/app?name=a.exe&limit=20");
    assert_eq!(status_code(&head), 200);
    let value: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|_| panic!("drill-down must be valid JSON, got: {body}"));
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("drill-down must be a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["connections", "destinations", "titles", "usage"],
        "drill-down must have exactly these four keys"
    );
    assert!(
        !value["destinations"].as_array().unwrap().is_empty(),
        "seeded a.exe must have at least one destination"
    );
    assert_eq!(value["usage"]["session_count"], 1);
    assert!(
        !value["titles"].as_array().unwrap().is_empty(),
        "seeded a.exe must have at least one title"
    );
    assert!(
        !value["connections"].as_array().unwrap().is_empty(),
        "seeded a.exe must have at least one connection"
    );
    // Ledger F9: the filter API accepts only lowercase (`protocol=tcp`|
    // `udp`, `direction=out`|`in`), so the response must speak the same
    // casing. The seed uses `Protocol::Tcp` + `Direction::Out`, which serde
    // would otherwise emit as "Tcp"/"Out".
    assert!(
        body.contains(r#""protocol":"tcp""#),
        "destinations/connections must lowercase protocol, body: {body}"
    );
    assert!(
        body.contains(r#""direction":"out""#),
        "connections must lowercase direction, body: {body}"
    );
    assert!(
        !body.contains(r#""protocol":"Tcp""#) && !body.contains(r#""direction":"Out""#),
        "no PascalCase enum values may survive the boundary, body: {body}"
    );
}

#[test]
fn bad_filter_is_400() {
    // parse_filter is pure, so the good/bad value assertions live here too.
    let filter = routes::parse_filter("protocol=udp&port=53&direction=in").expect("valid filter");
    assert_eq!(filter.protocol, Some(Protocol::Udp));
    assert_eq!(filter.port, Some(53));
    assert_eq!(filter.direction, Some(Direction::In));

    assert!(routes::parse_filter("").is_ok(), "empty query is ok: all");
    let default = routes::parse_filter("").unwrap();
    assert_eq!(default.from_ms, 0);
    assert_eq!(default.to_ms, u64::MAX);
    assert_eq!(default.app, None);
    assert_eq!(default.protocol, None);

    assert!(
        routes::parse_filter("protocol=xyz").is_err(),
        "bad protocol"
    );
    assert!(
        routes::parse_filter("direction=maybe").is_err(),
        "bad direction"
    );
    assert!(routes::parse_filter("port=70000").is_err(), "port over u16");
    assert!(
        routes::parse_filter("port=abc").is_err(),
        "non-numeric port"
    );
    assert!(
        routes::parse_filter("from=nope").is_err(),
        "non-numeric from"
    );
    assert!(routes::parse_filter("to=-5").is_err(), "negative to");

    // Over HTTP the same bad value becomes a 400 with a helpful body.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db");
    seed_db(&path);
    let (port, _stop, _done) = spawn_run(path);

    let (head, body) = raw_request(port, "GET", "/api/overview?protocol=xyz");
    assert_eq!(status_code(&head), 400);
    let lowered = body.to_ascii_lowercase();
    assert!(
        lowered.contains("protocol"),
        "400 body must say which parameter is wrong: {body}"
    );
}

#[test]
fn missing_db_is_503_then_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db"); // deliberately absent at first
    let (port, _stop, _done) = spawn_run(path.clone());

    let (head, body) = raw_request(port, "GET", "/api/overview");
    assert_eq!(status_code(&head), 503);
    assert_eq!(body, NOT_READY_BODY);

    seed_db(&path); // "the agent starts" — the store open must be retried

    let (head, body) = raw_request(port, "GET", "/api/overview");
    assert_eq!(status_code(&head), 200);
    assert!(body.contains("a.exe"), "recovered body: {body}");
}

#[test]
fn unknown_route_404s() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db");
    seed_db(&path);
    let (port, _stop, _done) = spawn_run(path);

    // Positive control: the static UI route exists and is HTML...
    let (head, _) = raw_request(port, "GET", "/");
    assert_eq!(status_code(&head), 200);
    assert!(
        head.to_ascii_lowercase().contains("text/html"),
        "GET / must be text/html, head: {head}"
    );

    // ...while unknown routes 404, with "404" in the status line.
    let (head, _) = raw_request(port, "GET", "/definitely-not-a-route");
    let status_line = head.lines().next().expect("status line").to_string();
    assert!(
        status_line.contains("404"),
        "status line should carry 404: {status_line}"
    );
    assert_eq!(status_code(&head), 404);
}

#[test]
fn shutdown_endpoint_stops_server() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db");
    seed_db(&path);
    let (port, stop, done) = spawn_run(path);

    let (head, _) = raw_request(port, "POST", "/api/shutdown");
    assert_eq!(status_code(&head), 200, "shutdown must answer 200 first");

    let result = done
        .recv_timeout(Duration::from_secs(2))
        .expect("run() must return within 2s of the shutdown POST");
    result.expect("run() must return Ok on shutdown");
    assert!(
        stop.load(Ordering::Relaxed),
        "the endpoint must set the shared stop flag"
    );
}

#[test]
fn ui_has_no_external_urls() {
    let ui = include_str!("../src/ui.html").replace("http://www.w3.org/2000/svg", "");
    assert!(!ui.contains("http://") && !ui.contains("https://") && !ui.contains("//cdn"));
}
