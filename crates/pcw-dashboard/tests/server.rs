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

// Export menu wiring (SDD 2026-10-26-pc-watcher-export, task 3): the UI must
// fetch its downloads from the two server routes by their literal paths,
// sending the same filter query the view uses, so an export always matches
// the current view.
#[test]
fn ui_pins_export_paths() {
    let ui = include_str!("../src/ui.html");
    assert!(
        ui.contains("/api/export/xlsx"),
        "ui.html must wire the xlsx export route"
    );
    assert!(
        ui.contains("/api/export/csv"),
        "ui.html must wire the csv export route"
    );
}

#[test]
fn ui_has_no_external_urls() {
    let ui = include_str!("../src/ui.html").replace("http://www.w3.org/2000/svg", "");
    assert!(!ui.contains("http://") && !ui.contains("https://") && !ui.contains("//cdn"));
}

#[test]
fn ui_default_dates_use_local_components() {
    let ui = include_str!("../src/ui.html");
    // Regression: `valueAsDate` interprets UTC calendar components, so on
    // timezones east of UTC (e.g. UTC+5:30) the default `from`/`to` inputs
    // stored yesterday's date and the default query returned zero rows.
    // The fix builds a local "YYYY-MM-DD" string and assigns `.value`.
    assert!(
        !ui.contains("valueAsDate"),
        "ui.html must not use valueAsDate (UTC-calendar interpretation bug)"
    );
    assert!(
        ui.contains("getFullYear"),
        "ui.html must construct the default dates from local date components"
    );
}

// Export download routes (SDD 2026-10-26-pc-watcher-export, task 2).

/// Raw request returning (head, body bytes) — the exports are binary files,
/// so unlike [`raw_request`] the body must survive as bytes.
fn raw_request_bytes(port: u16, method: &str, target: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to dashboard");
    let request =
        format!("{method} {target} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP head/body separator");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    (head, raw[split + 4..].to_vec())
}

/// The export response head must carry the exact attachment header with a
/// `pcw-export-<YYYY-MM-DD>.<ext>` filename, and the Content-Type the route
/// promises. Returns the extracted filename date (`YYYY-MM-DD`).
fn assert_download_head(head: &str, content_type: &str, ext: &str) -> String {
    assert_eq!(status_code(head), 200);
    let lowered = head.to_ascii_lowercase();
    assert!(
        lowered.contains(content_type),
        "content-type must be {content_type}, head: {head}"
    );
    let expected = "content-disposition: attachment; filename=pcw-export-";
    let Some(date) = lowered
        .split(expected)
        .nth(1)
        .map(|rest| rest[..10].to_string())
    else {
        panic!("missing Content-Disposition head: {head}");
    };
    assert_eq!(
        lowered.matches("content-disposition:").count(),
        1,
        "exactly one content-disposition line, head: {head}"
    );
    assert!(
        lowered.contains(&format!(
            "content-disposition: attachment; filename=pcw-export-{date}.{ext}"
        )),
        "head: {head}"
    );
    assert!(
        date.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 4 | 7) {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        }),
        "filename date must be YYYY-MM-DD, got {date:?}"
    );
    date
}

/// The zip container of either export (xlsx is zip too), parsed in memory.
fn unzip(body: &[u8]) -> zip::ZipArchive<std::io::Cursor<&[u8]>> {
    assert_eq!(&body[..2], b"PK", "zip container magic missing");
    zip::ZipArchive::new(std::io::Cursor::new(body)).expect("valid zip container")
}

/// Text of one entry of a parsed zip archive.
fn zip_text<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> String {
    let mut text = String::new();
    archive
        .by_name(name)
        .expect("entry exists")
        .read_to_string(&mut text)
        .expect("entry is UTF-8 text");
    text
}

/// First line of one entry (its CSV header row).
fn first_line<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> String {
    zip_text(archive, name)
        .lines()
        .next()
        .expect("CSV has a header row")
        .to_string()
}

/// The shared-strings table of an xlsx, in order (rust_xlsxwriter stores
/// cell strings there; the `xml:space` variant is normalized first).
fn shared_strings<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> Vec<String> {
    let xml =
        zip_text(archive, "xl/sharedStrings.xml").replace("<t xml:space=\"preserve\">", "<t>");
    xml.split("<t>")
        .skip(1)
        .map(|rest| rest[..rest.find("</t>").expect("closed string")].to_string())
        .collect()
}

/// The first `count` cell values of one worksheet, resolved through the
/// shared-strings table (xlsx stores strings as shared-string indices);
/// row 1 comes first in the sheet XML, so these are the header cells.
fn first_cells<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    sheet: &str,
    shared: &[String],
    count: usize,
) -> Vec<String> {
    let xml = zip_text(archive, sheet);
    let mut out = Vec::new();
    let mut pos = 0;
    while out.len() < count {
        let start = xml[pos..].find("<v>").expect("cell value") + pos + 3;
        let end = xml[start..].find("</v>").expect("cell value end") + start;
        let index: usize = xml[start..end].parse().expect("shared-string index");
        out.push(shared[index].clone());
        pos = end;
    }
    out
}

#[test]
fn export_xlsx_serves_workbook_with_four_sheets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db");
    seed_db(&path);
    let (port, _stop, _done) = spawn_run(path);

    let (head, body) = raw_request_bytes(port, "GET", "/api/export/xlsx");
    assert_download_head(
        &head,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx",
    );
    let mut archive = unzip(&body);
    // The workbook parses and carries the four sheets in the brief's order.
    let workbook_xml = zip_text(&mut archive, "xl/workbook.xml");
    let mut order = 0;
    for name in ["Manifest", "Connections", "Sessions", "Titles"] {
        let at = workbook_xml
            .find(&format!("name=\"{name}\""))
            .unwrap_or_else(|| panic!("sheet {name} missing from workbook.xml: {workbook_xml}"));
        assert!(at >= order, "sheets out of order around {name}");
        order = at;
    }
    // Sheet headers carry every schema column, `id` included (sheets are
    // written in creation order: sheet1=Manifest … sheet4=Titles).
    let shared = shared_strings(&mut archive);
    assert_eq!(
        first_cells(&mut archive, "xl/worksheets/sheet2.xml", &shared, 2),
        ["id", "ts_first"]
    );
    assert_eq!(
        first_cells(&mut archive, "xl/worksheets/sheet3.xml", &shared, 2),
        ["id", "ts_start"]
    );
    assert_eq!(
        first_cells(&mut archive, "xl/worksheets/sheet4.xml", &shared, 2),
        ["id", "ts"]
    );
}

#[test]
fn export_csv_zips_exactly_the_four_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db");
    seed_db(&path);
    let (port, _stop, _done) = spawn_run(path);

    let (head, body) = raw_request_bytes(port, "GET", "/api/export/csv");
    assert_download_head(&head, "application/zip", "zip");
    let mut archive = unzip(&body);
    let mut names: Vec<&str> = archive.file_names().collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "connections.csv",
            "manifest.json",
            "sessions.csv",
            "titles.csv"
        ],
        "the zip holds exactly the four brief files, got {names:?}"
    );
    // Every schema column ships, header row verbatim — `id` included
    // (regression guard: a dropped column fails here).
    assert_eq!(
        first_line(&mut archive, "connections.csv"),
        "id,ts_first,ts_last,pid,process_name,process_path,remote_ip,remote_port,local_port,protocol,direction,hostname"
    );
    assert_eq!(
        first_line(&mut archive, "sessions.csv"),
        "id,ts_start,ts_end,process_name,window_title"
    );
    assert_eq!(
        first_line(&mut archive, "titles.csv"),
        "id,ts,process_name,title"
    );
}

#[test]
fn export_manifest_carries_version_range_filters_counts_and_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db");
    seed_db(&path);
    let (port, _stop, _done) = spawn_run(path);

    let (_head, body) = raw_request_bytes(port, "GET", "/api/export/csv");
    let mut archive = unzip(&body);
    let manifest: serde_json::Value =
        serde_json::from_str(&zip_text(&mut archive, "manifest.json")).expect("manifest.json");
    let obj = manifest.as_object().unwrap();

    assert_eq!(
        manifest["tool_version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    // Export timestamp with a local timezone: RFC 3339-like, offset-annotated.
    let generated = manifest["generated_at"].as_str().expect("generated_at");
    assert!(
        generated.len() > 20 && generated.as_bytes()[4] == b'-',
        "generated_at: {generated:?}"
    );

    assert_eq!(
        manifest["from_ms"].as_u64(),
        Some(0),
        "default filter starts at 0"
    );
    assert!(manifest["to_ms"].is_u64(), "to_ms present");
    assert!(manifest["from_local"].is_string(), "readable from present");
    assert!(manifest["to_local"].is_string(), "readable to present");

    for key in [
        "filter_app",
        "filter_protocol",
        "filter_port",
        "filter_direction",
    ] {
        assert_eq!(manifest[key].as_str(), Some("any"), "unfiltered {key}");
    }
    assert_eq!(manifest["rows_connections"].as_u64(), Some(2));
    assert_eq!(manifest["rows_sessions"].as_u64(), Some(1));
    assert_eq!(manifest["rows_titles"].as_u64(), Some(1));
    assert!(
        manifest["source_db"]
            .as_str()
            .is_some_and(|db| db.ends_with("watcher.db")),
        "source db path: {obj:?}"
    );

    // Row-level detail: the hostname join ships like in the JSON endpoints.
    let connections = zip_text(&mut archive, "connections.csv");
    assert!(
        connections.contains("example.test"),
        "resolved hostname must ship, connections.csv: {connections}"
    );
}

#[test]
fn export_csv_honors_app_filter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db");
    seed_db(&path);
    // A second app the filter must exclude from every dataset.
    let mut store = Store::open(&path).expect("reopen seeded db");
    let now = now_ms();
    store
        .apply_batch(
            &[
                WriteOp::Connection(ConnectionEvent {
                    ts_ms: now,
                    kind: EventKind::Close,
                    pid: 2001,
                    process_name: "b.exe".into(),
                    process_path: r"C:\tests\b.exe".into(),
                    remote_ip: "255.255.255.250".into(),
                    remote_port: 8080,
                    local_port: 52001,
                    protocol: Protocol::Tcp,
                    direction: Direction::Out,
                }),
                WriteOp::Session(AppSession {
                    ts_start_ms: now - 10_000,
                    ts_end_ms: now,
                    process_name: "b.exe".into(),
                    window_title: "b-window".into(),
                }),
                WriteOp::Title(WindowTitle {
                    ts_ms: now - 5_000,
                    process_name: "b.exe".into(),
                    title: "b-title".into(),
                }),
            ],
            now,
        )
        .expect("seed b.exe rows");
    drop(store);
    let (port, _stop, _done) = spawn_run(path);

    let (_head, body) = raw_request_bytes(port, "GET", "/api/export/csv?app=a.exe");
    let mut archive = unzip(&body);
    let manifest: serde_json::Value =
        serde_json::from_str(&zip_text(&mut archive, "manifest.json")).unwrap();
    assert_eq!(manifest["filter_app"].as_str(), Some("a.exe"));
    assert_eq!(
        manifest["rows_connections"].as_u64(),
        Some(2),
        "b.exe excluded"
    );
    assert_eq!(manifest["rows_sessions"].as_u64(), Some(1));
    assert_eq!(manifest["rows_titles"].as_u64(), Some(1));

    for (file, b_marker) in [
        ("connections.csv", "b.exe"),
        ("sessions.csv", "b-window"),
        ("titles.csv", "b-title"),
    ] {
        let text = zip_text(&mut archive, file);
        assert!(text.contains("a.exe"), "{file} keeps the filtered app");
        assert!(
            !text.contains(b_marker),
            "{file} must not carry b.exe rows: {text}"
        );
    }
}

#[test]
fn export_empty_range_returns_valid_files_with_zero_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db");
    seed_db(&path);
    let (port, _stop, _done) = spawn_run(path);

    let from = now_ms() + 7 * 86_400_000; // a week ahead: nothing was seeded there
    let to = from + 3_600_000;

    let (_head, body) =
        raw_request_bytes(port, "GET", &format!("/api/export/csv?from={from}&to={to}"));
    let mut archive = unzip(&body);
    let manifest: serde_json::Value =
        serde_json::from_str(&zip_text(&mut archive, "manifest.json")).unwrap();
    assert_eq!(manifest["from_ms"].as_u64(), Some(from));
    assert_eq!(manifest["rows_connections"].as_u64(), Some(0));
    assert_eq!(manifest["rows_sessions"].as_u64(), Some(0));
    assert_eq!(manifest["rows_titles"].as_u64(), Some(0));
    for file in ["connections.csv", "sessions.csv", "titles.csv"] {
        let text = zip_text(&mut archive, file);
        assert_eq!(
            text.lines().count(),
            1,
            "empty range keeps only the header row in {file}: {text:?}"
        );
        assert!(!text.contains("a.exe"), "{file} must carry no seeded rows");
    }

    let (head, body) = raw_request_bytes(
        port,
        "GET",
        &format!("/api/export/xlsx?from={from}&to={to}"),
    );
    assert_download_head(
        &head,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx",
    );
    assert_eq!(&body[..2], b"PK", "empty xlsx is still a valid container");
}

#[test]
fn export_bad_filter_is_400() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watcher.db");
    seed_db(&path);
    let (port, _stop, _done) = spawn_run(path);

    for (route, message) in [
        ("/api/export/xlsx?protocol=xyz", "protocol"),
        ("/api/export/csv?port=99999", "port"),
        ("/api/export/xlsx?from=nope", "from"),
    ] {
        let (head, body) = raw_request(port, "GET", route);
        assert_eq!(
            status_code(&head),
            400,
            "{route} must be a 400, head: {head}"
        );
        assert!(
            body.to_ascii_lowercase().contains(message),
            "400 body must name the bad parameter, body: {body}"
        );
    }
}
