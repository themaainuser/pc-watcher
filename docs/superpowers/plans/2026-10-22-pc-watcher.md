# PC-Watcher Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an always-on, lightweight Windows tray app that records per-process network connection metadata, service names and foreground app usage (with every window-title change) into a local SQLite DB, with an on-demand localhost dashboard and weekly structured export to a Google Drive folder.

**Architecture:** A Rust cargo workspace of ten small crates producing two binaries: `pcw-agent` (elevated logon scheduled task; one ferrisetw ETW session for Kernel-Network + DNS-Client + Kernel-Process, WinEvent foreground hooks, batched SQLite writer, pruner, exporter, tray) and `pcw-dash` (on-demand read-only server on 127.0.0.1 serving one embedded HTML file). TCP = one row per connection (open → close); UDP = one row per datagram. All writes go through one writer thread that owns `SharedStore = Arc<Mutex<Store>>`.

**Tech Stack:** Rust 1.99 stable (`x86_64-pc-windows-msvc`), `ferrisetw` 1.2.0, `windows` 0.62.2 (only in `pcw-etw`/`pcw-foreground`), `rusqlite` 0.40.2 (bundled), `tiny_http` 0.12.0, `tray-icon` 0.26.1 + `tao` 0.37.1, `dns-lookup` 4.0.2, `serde`/`serde_json`/`toml`/`csv`/`time`/`log`, `rust-lld` linker.

**Spec:** `docs/superpowers/specs/2026-10-21-pc-watcher-design.md` (revised 2026-10-22) — the plan argues from the spec; executors read both.

**Verified 2026-10-22 on this PC:** every crate version, `windows` feature, Win32 symbol, ETW provider GUID, event ID and event field name in this plan was checked: a throwaway crate using them builds and links with `rust-lld`. Do not swap in other names without re-checking.

## Before Task 1 (owner action)

The project folder is under the OneDrive-synced Desktop (`C:\Users\Kartik\OneDrive\Desktop\PC-Watcher`). Before building, the owner moves it out (for example `C:\dev\PC-Watcher`) — or, at minimum, sets the user environment variable `CARGO_TARGET_DIR` to a folder outside OneDrive. Executors never move the folder themselves; Task 1 Step 1 stops if this has not been done.

## Global Constraints

- Platform: Windows 10/11 x64 only. Workspace `edition = "2024"`, `rust-version = "1.85"`.
- Metadata only: never collect payloads, URLs, HTTP headers, keystrokes, screenshots, file contents, or command lines. Window titles: captured, every change (spec §4.3). DNS: only names for IPs that appear in recorded connections; no DNS query log (spec §3, §4.5).
- TCP = one row per connection, open → close. UDP = one row per datagram. No other merging (spec §4.4).
- All timestamps are Unix epoch milliseconds, UTC, as `u64` in Rust and `INT` in SQLite.
- DB path: `%LOCALAPPDATA%\PC-Watcher\data.db`; env override `PCW_DATA_DIR` — never inside a sync root (spec §4.6).
- Dashboard binds `127.0.0.1` only, default port `7411`; the embedded HTML references no external URL except the SVG namespace string `http://www.w3.org/2000/svg` (spec §5.2).
- Config file `%LOCALAPPDATA%\PC-Watcher\config.toml` with exact defaults: `retention_days = 10`, `resolve_dns = true`, `capture_dns_names = true`, `capture_window_titles = true`, `export_folder = "G:\\My Drive\\PC-Watcher"`, `export_schedule = "weekly"`, `dashboard_port = 7411`, `udp_rate_cap = 0`, `min_free_disk_gb = 2` (spec §8).
- Pruner deletes a row only if its id ≤ that table's export watermark **and** it is older than `retention_days`; no watermark ⇒ nothing deleted (spec §4.7).
- Session is open only while not locked, not asleep, display not off, not paused; `LockApp.exe` never gets a session; idle does NOT end a session (spec §4.3).
- `windows` crate is imported only in `pcw-etw` and `pcw-foreground`, with explicit feature lists (spec §11.4).
- Root `[workspace.dependencies]` pins every direct dependency once; members use `dep = { workspace = true }`, adding features where needed (spec §11.2).
- Release profile: `lto = "thin"`, `opt-level = 3`, `strip = true` (spec §11.6).
- Both binaries start with `#![windows_subsystem = "windows"]` (spec §5).
- Agent install: task `PC-Watcher` registered from XML with `HighestAvailable`, no battery restrictions, `ExecutionTimeLimit PT0S`, binaries copied to `%LOCALAPPDATA%\PC-Watcher\bin\` (spec §6).
- Tray menu exactly: Open Dashboard, Pause / Resume (menu label toggles between "Pause" and "Resume"), Export now, Open data folder, Exit (spec §5.1).
- Meta keys exactly: `schema_version`, `last_export`, `export_wm_connections`, `export_wm_sessions`, `export_wm_titles`, `open_session`, and the six counters `etw_events_lost`, `etw_buffers_lost`, `watchdog_restarts`, `udp_capped`, `udp_skipped_low_disk`, `rows_dropped_disk_full`.
- Footprint targets (manual smoke): agent < 40 MB RAM, < 1% average CPU idle (spec §12).
- Conventional commits (`feat:`, `fix:`, `test:`, `chore:`, `docs:`); every task ends with a commit.

## Review Focus

Failure modes the spec implies that most tasks don't exercise — each pinned to the task that owns it:

1. **Rows that reach the DB after an export, with timestamps older than the export, must not be pruned before they are exported** (they were still in the 1 s write batch) — data loss is unrecoverable. → Task 6 `keeps_old_rows_written_after_export`, Task 7 `rows_written_after_export_go_in_next_export`.
2. **Lock, sleep, display-off and pause must close the session; idle must not; waking or turning the display on while still locked, and the lock screen app, must never count as usage** — getting this wrong inflates daily usage. → Task 9 tracker tests.
3. **Dashboard must be unreachable from the network** — a mis-bound server would expose monitoring data to the LAN. → Task 14 `binds_loopback_only`, Task 15 LAN check.
4. **Window titles containing quotes, commas, tabs, and newlines must survive CSV export intact** — titles are free text and land in Drive spreadsheets. → Task 7 `hostile_window_title_survives_csv_roundtrip`.
5. **Per-datagram UDP filling the disk** — low free space must stop UDP rows, and a "disk full" write must not crash the agent or stop later TCP/session writes. → Task 4 `disk_full_is_reported_and_store_stays_usable`, Task 12 writer tests.

---

### Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml`, `.cargo/config.toml`, `.gitignore`, `README.md`
- Create: `crates/{pcw-types,pcw-config,pcw-storage,pcw-export,pcw-resolve,pcw-etw,pcw-foreground,pcw-dashboard}/Cargo.toml` + `src/lib.rs` stubs
- Create: `crates/pcw-agent/Cargo.toml` + `src/main.rs` + `src/lib.rs` stubs, `crates/pcw-dash/Cargo.toml` + `src/main.rs` stub

**Interfaces:**
- Produces: the workspace all later tasks build inside. Package names are exactly the directory names; `pcw-agent` (bin + lib) and `pcw-dash` (bin) are the only crates with binaries.

- [ ] **Step 1: Check the folder is outside OneDrive, then initialize git**

Run (PowerShell): `(Get-Location).Path` — Expected: the path does not contain `OneDrive`. If it does and `$env:CARGO_TARGET_DIR` is empty or itself under OneDrive, **stop and ask the owner** (see "Before Task 1").
Run: `git init`, create `.gitignore` containing `target/`, `*.db`, `*.db-wal`, `*.db-shm`, `*.log`. Then `git add -A && git commit -m "chore: init repo"`.

- [ ] **Step 2: Write the root `Cargo.toml`**

`[workspace] members = ["crates/*"]`, `resolver = "3"`; `[workspace.package] edition = "2024"`, `rust-version = "1.85"`, `version = "0.1.0"`. `[workspace.dependencies]`, exactly these versions:

```toml
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.154"
toml = "1.1.8"
thiserror = "2.0.21"
rusqlite = { version = "0.40.2", features = ["bundled"] }
csv = "1.4.0"
dns-lookup = "4.0.2"
ferrisetw = "1.2.0"
windows = "0.62.2"            # members list their own features (below)
tiny_http = "0.12.0"
tray-icon = "0.26.1"
tao = "0.37.1"
time = { version = "0.3.55", features = ["local-offset", "formatting", "macros"] }
log = "0.4.34"
tempfile = "3.27.0"           # dev-dependency only
```

`windows` features per member (verified):
- `pcw-etw`: `Win32_Foundation`, `Win32_System_Threading`, `Win32_System_Diagnostics_Etw`, `Win32_NetworkManagement_IpHelper`, `Win32_Networking_WinSock`
- `pcw-foreground`: `Win32_Foundation`, `Win32_System_Threading`, `Win32_UI_Accessibility`, `Win32_UI_WindowsAndMessaging`, `Win32_System_Power`, `Win32_System_RemoteDesktop`, `Win32_System_SystemServices`, `Win32_System_LibraryLoader`, `Win32_Graphics_Gdi`, `Win32_System_Console`, `Win32_Storage_FileSystem`, `Win32_Security`

Profiles: `[profile.release] lto = "thin"`, `opt-level = 3`, `strip = true`; `[profile.dev] incremental = true`.

- [ ] **Step 3: Write `.cargo/config.toml`**

```toml
[target.x86_64-pc-windows-msvc]
linker = "rust-lld"
```

- [ ] **Step 4: Scaffold the ten member crates**

Eight libraries (`pcw-types`, `pcw-config`, `pcw-storage`, `pcw-export`, `pcw-resolve`, `pcw-etw`, `pcw-foreground`, `pcw-dashboard`), `pcw-agent` with `src/lib.rs` + `src/main.rs`, `pcw-dash` with `src/main.rs`. Both `main.rs` files start with `#![windows_subsystem = "windows"]`. Each member uses `edition.workspace = true` etc. `README.md` documents: build (`cargo build --release`), `cargo build --timings` (spec §11.7), the `rust-lld` linker config, and the OneDrive rule.

- [ ] **Step 5: Build the workspace**

Run: `cargo build -v 2>&1 | Select-String "rust-lld" | Select-Object -First 3`
Expected: lines showing `-C linker=rust-lld`; `cargo build` finishes with no errors. Then `git add -A && git commit -m "chore: cargo workspace scaffold with rust-lld"`.

---

### Task 2: Shared types, process cache, counters

**Files:**
- Create: `crates/pcw-types/src/lib.rs`, `crates/pcw-types/src/process.rs`, `crates/pcw-types/src/counters.rs`
- Test: `crates/pcw-types/tests/types.rs`

**Interfaces:**
- Consumes: nothing.
- Produces (later tasks depend on these exact names; all re-exported at the crate root):
  - `pub enum Protocol { Tcp, Udp }`, `pub enum Direction { Out, In }`, `pub enum EventKind { Open, Close, Packet }` — all `Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize`. `Protocol` and `Direction` have `fn as_str(self) -> &'static str` (`"tcp"`/`"udp"`, `"out"`/`"in"`) and `fn parse(s: &str) -> Option<Self>`.
  - `pub struct ConnectionEvent { pub ts_ms: u64, pub kind: EventKind, pub pid: u32, pub process_name: String, pub process_path: String, pub remote_ip: String, pub remote_port: u16, pub local_port: u16, pub protocol: Protocol, pub direction: Direction }`
  - `pub struct AppSession { pub ts_start_ms: u64, pub ts_end_ms: u64, pub process_name: String, pub window_title: String }`
  - `pub struct WindowTitle { pub ts_ms: u64, pub process_name: String, pub title: String }`
  - `pub struct DnsAnswer { pub ts_ms: u64, pub name: String, pub ips: Vec<String> }`
  - All four structs: `Debug, Clone, PartialEq, Serialize, Deserialize`.
  - `pub struct ProcessCache` with `new(query: Box<dyn FnMut(u32) -> Option<(String, String)> + Send>) -> Self`; `get(&mut self, pid: u32) -> (String, String)` (name, path); `note_process(&mut self, pid: u32, image_path: &str, started: bool)`. `pub type SharedProcessCache = Arc<Mutex<ProcessCache>>`.
  - `pub fn file_name_of(path: &str) -> String` — text after the last `\` or `/` (works for `C:\…` and `\Device\HarddiskVolumeN\…`).
  - `pub struct Counters` (six `AtomicU64`s, `Default`), `pub const COUNTER_KEYS: [&str; 6] = ["etw_events_lost", "etw_buffers_lost", "watchdog_restarts", "udp_capped", "udp_skipped_low_disk", "rows_dropped_disk_full"]`, methods `add(&self, key: &str, n: u64)`, `set(&self, key: &str, v: u64)`, `snapshot(&self) -> Vec<(&'static str, u64)>` (in `COUNTER_KEYS` order).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn connection_event_roundtrips_json() { /* build one with kind Open, Tcp, Out; serde_json round trip == original */ }
#[test] fn protocol_and_direction_text_roundtrip() {
    assert_eq!(Protocol::parse(Protocol::Udp.as_str()), Some(Protocol::Udp));
    assert_eq!(Direction::In.as_str(), "in"); assert_eq!(Protocol::parse("xyz"), None);
}
#[test] fn process_cache_queries_once_then_caches() {
    let calls = Arc::new(AtomicUsize::new(0)); let c2 = calls.clone();
    let mut c = ProcessCache::new(Box::new(move |_| { c2.fetch_add(1, SeqCst); Some(("x.exe".into(), "C:\\x.exe".into())) }));
    c.get(7); c.get(7);
    assert_eq!(calls.load(SeqCst), 1);
}
#[test] fn failed_lookup_is_unknown_and_retried() {
    let fail = Arc::new(AtomicBool::new(true)); let f2 = fail.clone();   // shared flag: a `move`d bool would be a copy
    let mut c = ProcessCache::new(Box::new(move |_| if f2.load(SeqCst) { None } else { Some(("y.exe".into(), String::new())) }));
    assert_eq!(c.get(9).0, "unknown");
    fail.store(false, SeqCst);
    assert_eq!(c.get(9).0, "y.exe");
}
#[test] fn process_start_invalidates_reused_pid() {
    // query returns "a.exe" first, then "b.exe"; get(5) == a.exe; note_process(5, "C:\\b.exe", true); get(5) == b.exe
}
#[test] fn exited_process_falls_back_to_event_image() {
    // query always None; note_process(6, "\\Device\\HarddiskVolume3\\App\\z.exe", false); get(6).0 == "z.exe"
}
#[test] fn file_name_of_handles_dos_and_device_paths() {
    assert_eq!(file_name_of("C:\\a\\b.exe"), "b.exe");
    assert_eq!(file_name_of("\\Device\\HarddiskVolume3\\x\\y.exe"), "y.exe");
}
#[test] fn counters_snapshot_lists_all_six_keys_in_order() { /* add("udp_capped", 3) → snapshot()[3] == ("udp_capped", 3); len 6 */ }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-types` — Expected: FAIL (types not defined).

- [ ] **Step 3: Implement** — `ProcessCache` holds `HashMap<u32, (String, String)>` for successful lookups and `HashMap<u32, String>` for fallback image paths; failures are never cached; `note_process(.., started: true)` removes the cached entry, and both start and stop record the image path as fallback.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-types` — Expected: 8 passed.

- [ ] **Step 5: Commit** — `git add crates/pcw-types && git commit -m "feat: shared event types, process cache, counters"`.

---

### Task 3: Config and data directory

**Files:**
- Create: `crates/pcw-config/src/lib.rs`
- Test: `crates/pcw-config/tests/config.rs`

**Interfaces:**
- Produces:
  - `pub struct Config { pub retention_days: u32, pub resolve_dns: bool, pub capture_dns_names: bool, pub capture_window_titles: bool, pub export_folder: PathBuf, pub export_schedule: String, pub dashboard_port: u16, pub udp_rate_cap: u32, pub min_free_disk_gb: u32 }`
  - `pub enum ConfigError { Io(std::io::Error), Parse(String), NoDataDir }` (`thiserror`).
  - `pub fn data_dir_from(pcw_data_dir: Option<OsString>, local_app_data: Option<OsString>) -> Result<PathBuf, ConfigError>` — `PCW_DATA_DIR` value if set, else `<LOCALAPPDATA>\PC-Watcher`, else `NoDataDir`. Pure, so tests never mutate the process environment (`set_var` is `unsafe` in edition 2024).
  - `pub fn default_data_dir() -> Result<PathBuf, ConfigError>` — calls `data_dir_from(env::var_os("PCW_DATA_DIR"), env::var_os("LOCALAPPDATA"))`.
  - `impl Config { pub fn load_or_create(data_dir: &Path) -> Result<Config, ConfigError> }` — creates `data_dir` if missing; writes the spec §8 defaults (with their comments) to `config.toml` when absent; parses; missing keys take defaults; unknown keys tolerated.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn creates_default_config_with_spec_values() {
    let dir = tempfile::tempdir().unwrap(); let cfg = Config::load_or_create(dir.path()).unwrap();
    assert_eq!(cfg.retention_days, 10); assert!(cfg.resolve_dns); assert!(cfg.capture_dns_names);
    assert!(cfg.capture_window_titles);
    assert_eq!(cfg.export_folder, PathBuf::from("G:\\My Drive\\PC-Watcher"));
    assert_eq!(cfg.export_schedule, "weekly"); assert_eq!(cfg.dashboard_port, 7411);
    assert_eq!(cfg.udp_rate_cap, 0); assert_eq!(cfg.min_free_disk_gb, 2);
    assert!(dir.path().join("config.toml").exists());
}
#[test] fn reloads_edited_values() { /* write "retention_days = 5", reload → 5, other keys default */ }
#[test] fn invalid_toml_is_parse_error() { /* write "not = = toml" → Err(ConfigError::Parse(_)) */ }
#[test] fn data_dir_env_override_wins() {
    assert_eq!(data_dir_from(Some("D:\\x".into()), Some("C:\\L".into())).unwrap(), PathBuf::from("D:\\x"));
    assert_eq!(data_dir_from(None, Some("C:\\L".into())).unwrap(), PathBuf::from("C:\\L\\PC-Watcher"));
    assert!(matches!(data_dir_from(None, None), Err(ConfigError::NoDataDir)));
}
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-config` — Expected: FAIL.

- [ ] **Step 3: Implement** — `serde::Deserialize` on `Config` with `#[serde(default)]` at struct level and a `Default` impl holding the spec values.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-config` — Expected: 4 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: config with spec defaults and data-dir override"`.

---

### Task 4: Storage write path

**Files:**
- Create: `crates/pcw-storage/src/lib.rs`, `crates/pcw-storage/src/schema.rs` (`const SCHEMA: &str`), `crates/pcw-storage/src/write.rs`
- Test: `crates/pcw-storage/tests/write.rs`

**Interfaces:**
- Consumes: `pcw_types::{ConnectionEvent, AppSession, WindowTitle, DnsAnswer, EventKind, Protocol, Direction}`.
- Produces:
  - `pub struct Store`, `pub type SharedStore = Arc<Mutex<Store>>`, `pub enum StorageError { Sqlite(rusqlite::Error), DiskFull, Callback(std::io::Error) }`.
  - `Store::open(path: &Path) -> Result<Store, StorageError>` — creates parent dirs; `PRAGMA journal_mode=WAL`, `busy_timeout = 5000`; idempotent schema; `meta.schema_version = "1"`.
  - `Store::open_readonly(path: &Path) -> Result<Store, StorageError>` — `file:<path>?mode=ro` URI; any write through it returns `Err`.
  - `pub enum WriteOp { Connection(ConnectionEvent), Session(AppSession), Title(WindowTitle), Dns(DnsAnswer) }`
  - `Store::apply_batch(&mut self, ops: &[WriteOp], now_ms: u64) -> Result<u64, StorageError>` — one transaction; returns rows inserted or updated. On `SQLITE_FULL` the transaction is rolled back, in-memory state from that batch is discarded, and it returns `StorageError::DiskFull`.
  - `Store::set_meta(&mut self, key: &str, val: &str) -> Result<(), StorageError>`, `get_meta(&self, key: &str) -> Result<Option<String>, StorageError>`, `delete_meta(&mut self, key: &str) -> Result<(), StorageError>`.
  - `#[doc(hidden)] pub fn conn_for_tests(&mut self) -> &mut rusqlite::Connection` (tests only, e.g. `PRAGMA max_page_count`).
  - Schema exactly as spec §4.6: tables `connections`, `app_sessions`, `window_titles`, `hostnames(ip, hostname, source, resolved_at)`, `meta`, and the seven indexes listed there.
- `apply_batch` rules (spec §4.4, §4.5):
  - **TCP Open:** key `(local_port, remote_ip, remote_port)` not in the open map → insert row (`ts_first = ts_last = ts_ms`), map key → (row id, ts_ms); key present → ignore.
  - **TCP Close:** key present → `UPDATE ts_last`, remove key; absent → insert row with `ts_first = ts_last = ts_ms` using the event's direction.
  - **UDP Packet:** always insert a row.
  - Every Connection op whose `remote_ip` is not in the in-memory seen-IP set (cleared when it reaches 100,000) runs `INSERT OR IGNORE INTO hostnames`: `('dns', name, now_ms)` if the in-memory DNS map has the IP, else `(NULL, 'pending', 0)`.
  - **Dns:** for each ip: put ip → name in the DNS map (cap 50,000; drop oldest by insertion order), and `UPDATE hostnames SET hostname = ?, source = 'dns', resolved_at = ? WHERE ip = ?` (only IPs already recorded are touched).
  - Open-map entries older than 24 h are dropped, checked at most once an hour.
  - The open map, seen-IP set and DNS map start empty on `open()`.

- [ ] **Step 1: Write the failing tests**

Helper `tcp(kind, ts, local_port, ip, port)` and `udp(ts, ip, port)` builders; helper `count(store, table)`.

```rust
#[test] fn open_creates_schema_and_version() { /* meta schema_version == "1"; 5 tables exist in sqlite_master */ }
#[test] fn tcp_open_then_close_is_one_row() { /* Open@1000, Close@5000 same key → 1 row, ts_first 1000, ts_last 5000 */ }
#[test] fn close_frees_key_so_reconnect_is_new_row() { /* Open, Close, Open same key → 2 rows */ }
#[test] fn close_without_open_inserts_row() { /* Close@7000 alone → 1 row, ts_first == ts_last == 7000 */ }
#[test] fn udp_packets_never_merge() { /* two identical Packet ops → 2 rows */ }
#[test] fn new_ip_gets_pending_row_and_dns_answer_names_it() {
    // batch 1: Packet to 9.9.9.9 → hostnames row source 'pending'
    // batch 2: Dns{name:"dns.quad9.net", ips:["9.9.9.9"]} → source 'dns', hostname "dns.quad9.net"
    // batch 3: Dns for 1.1.1.1 (never seen) then Packet to 1.1.1.1 → row source 'dns' immediately
}
#[test] fn dns_answer_for_unrecorded_ip_stores_nothing() { /* Dns for 8.8.8.8 only → hostnames empty */ }
#[test] fn sessions_and_titles_persist_across_reopen() { /* apply, drop, Store::open again, counts 1 and 1, title text equal */ }
#[test] fn readonly_store_rejects_writes() { /* open, then open_readonly; apply_batch → Err */ }
#[test] fn disk_full_is_reported_and_store_stays_usable() {
    // Review Focus #5: PRAGMA max_page_count = <current page_count + 1>; apply a batch of 5,000 udp packets
    // → Err(StorageError::DiskFull); raise max_page_count; apply one TCP Open → Ok, row present
}
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-storage` — Expected: FAIL.

- [ ] **Step 3: Implement schema + `apply_batch`** per the Interfaces block, with prepared cached statements (`prepare_cached`) — this path runs for every datagram.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-storage` — Expected: 10 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: sqlite write path with tcp lifecycle, udp rows, dns names"`.

---

### Task 5: Storage read path and WAL concurrency

**Files:**
- Create: `crates/pcw-storage/src/read.rs`
- Test: `crates/pcw-storage/tests/read.rs`

**Interfaces:**
- Produces (consumed by Tasks 7, 8, 14):
  - `pub struct ConnFilter { pub from_ms: u64, pub to_ms: u64, pub app: Option<String>, pub protocol: Option<Protocol>, pub port: Option<u16>, pub direction: Option<Direction> }` (`Default`: `from_ms 0`, `to_ms u64::MAX`, rest `None`). `port` matches `remote_port` or `local_port`.
  - `pub struct ConnectionRow { pub id: i64, pub ts_first_ms: u64, pub ts_last_ms: u64, pub pid: u32, pub process_name: String, pub process_path: String, pub remote_ip: String, pub remote_port: u16, pub local_port: u16, pub protocol: Protocol, pub direction: Direction, pub hostname: Option<String> }` (`Serialize`)
  - `pub struct OverviewSummary { pub total_connections: u64, pub top_apps: Vec<(String, u64)>, pub top_services: Vec<(String, u64)>, pub hourly: Vec<(u8, u64)> }` — top 10 each; service = hostname if known else IP; hourly = local hour 0..23 (`strftime('%H', ts_first/1000, 'unixepoch', 'localtime')`).
  - `pub struct DestinationRow { pub remote_ip: String, pub remote_port: u16, pub protocol: Protocol, pub hostname: Option<String>, pub count: u64, pub ts_first_ms: u64, pub ts_last_ms: u64 }`
  - `pub struct UsageSummary { pub session_count: u64, pub total_secs: u64 }`
  - On `Store`: `overview(&self, f: &ConnFilter) -> Result<OverviewSummary, _>`; `app_destinations(&self, f: &ConnFilter) -> Result<Vec<DestinationRow>, _>` (`f.app` required); `connections(&self, f: &ConnFilter, limit: u32) -> Result<Vec<ConnectionRow>, _>` (newest first); `service_connections(&self, key: &str, f: &ConnFilter, limit: u32) -> Result<Vec<ConnectionRow>, _>` (hostname = key **or** remote_ip = key); `sessions(&self, from_ms: u64, to_ms: u64, app: Option<&str>) -> Result<Vec<AppSession>, _>`; `titles(&self, from_ms: u64, to_ms: u64, app: Option<&str>) -> Result<Vec<WindowTitle>, _>`; `usage_summary(&self, app: &str, from_ms: u64, to_ms: u64) -> Result<UsageSummary, _>`.
  - Hostname worker support: `pending_hostnames(&self, limit: u32, now_ms: u64) -> Result<Vec<String>, _>` — `source = 'pending'` first, then `source = 'none' AND resolved_at <= now_ms - 86_400_000`, oldest `resolved_at` first; `set_ptr_result(&mut self, ip: &str, name: Option<&str>, now_ms: u64) -> Result<(), _>` — `Some` ⇒ `'ptr'`, `None` ⇒ `'none'`; never changes a row whose source is `'dns'`.
  - Export support (streaming, id order, no full load into memory): `for_each_connection_after(&self, after_id: i64, f: &mut dyn FnMut(&ConnectionRow) -> std::io::Result<()>) -> Result<i64, StorageError>`, `for_each_session_after(&self, after_id: i64, f: &mut dyn FnMut(i64, &AppSession) -> std::io::Result<()>) -> Result<i64, _>`, `for_each_title_after(&self, after_id: i64, f: &mut dyn FnMut(i64, &WindowTitle) -> std::io::Result<()>) -> Result<i64, _>` — each returns the highest id passed to `f` (or `after_id` if none); a callback error becomes `StorageError::Callback`.

- [ ] **Step 1: Write the failing tests**

Seed helper: apps `a.exe` (3 TCP + 2 UDP) / `b.exe` (1 TCP), IPs `1.1.1.1` / `2.2.2.2`, ports 443/53, fixed timestamps at local hours 9 and 10, two sessions and three titles.

```rust
#[test] fn overview_counts_orders_and_buckets_hours() { /* total 6; top_apps[0] == ("a.exe", 5); hourly has hours 9 and 10 */ }
#[test] fn filters_by_protocol_port_direction() { /* protocol Udp → 2; port 53 → only port-53 rows; direction In → only In rows */ }
#[test] fn app_destinations_group_by_tuple() { /* a.exe → one DestinationRow per ip:port:protocol with counts */ }
#[test] fn service_connections_match_hostname_or_ip() { /* name 1.1.1.1 via Dns op; query "one.one.one.one" and "1.1.1.1" → same rows */ }
#[test] fn usage_summary_and_titles_for_app() { /* session_count 2, total_secs exact; titles(app) returns 3 in time order */ }
#[test] fn pending_hostnames_skip_recent_failures() {
    // ip A pending, ip B set_ptr_result(None, now) → pending_hostnames(10, now) == [A];
    // at now + 86_400_000 → contains B; an ip with source 'dns' never appears; set_ptr_result on a 'dns' ip leaves it 'dns'
}
#[test] fn for_each_connection_after_streams_in_id_order() { /* after_id = 2 → ids 3.. ascending; returns max id */ }
#[test] fn reader_does_not_block_while_writer_writes() {
    // two Stores on one file (open + open_readonly); writer thread applies 100 batches;
    // reader loops overview(); assert ≥ 1 successful read during writes and zero Err from reads
}
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-storage` — Expected: new tests FAIL, Task 4 tests still pass.

- [ ] **Step 3: Implement the query methods** — every connection query has `WHERE ts_first >= ?1 AND ts_first < ?2` and uses the spec §4.6 indexes; hostnames via `LEFT JOIN hostnames USING (ip)` on `remote_ip`; no unbounded `SELECT *`.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-storage` — Expected: 18 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: storage queries, filters, streaming readers, wal read test"`.

---

### Task 6: Retention pruner with export watermarks

**Files:**
- Create: `crates/pcw-storage/src/prune.rs`
- Test: `crates/pcw-storage/tests/prune.rs`

**Interfaces:**
- Consumes: `Store::get_meta`, schema from Task 4.
- Produces: `Store::prune_step(&mut self, retention_days: u32, now_ms: u64, max_rows: u32) -> Result<u64, StorageError>` — in one transaction, deletes up to `max_rows` rows in total from `connections` (by `ts_first`), `app_sessions` (by `ts_start`) and `window_titles` (by `ts`) where `id <= watermark(table)` **and** timestamp `< now_ms - retention_days * 86_400_000`. Watermarks: `meta.export_wm_connections`, `export_wm_sessions`, `export_wm_titles`; absent or not a valid `i64` ⇒ 0 ⇒ nothing deleted from that table. Returns rows deleted. `hostnames` is never touched. Callers loop until it returns 0 (Task 12 uses `max_rows = 50_000`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn deletes_old_exported_rows() { /* rows 11 days old, watermarks = max ids → all deleted */ }
#[test] fn keeps_old_rows_written_after_export() {
    // Review Focus #1: rows 11 days old with ids 1..=5, watermark = 3 → ids 4 and 5 remain
}
#[test] fn without_export_prunes_nothing() { /* rows years old, no watermark meta → all remain; corrupt "abc" watermark → all remain */ }
#[test] fn respects_chunk_size() { /* 25 eligible rows, max_rows 10 → returns 10, 10, 5, 0 */ }
#[test] fn never_prunes_hostname_cache() { /* hostnames rows survive a full prune */ }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-storage` — Expected: FAIL.

- [ ] **Step 3: Implement `prune_step`** — `DELETE … WHERE id IN (SELECT id … LIMIT ?)` per table, sharing the `max_rows` budget.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-storage` — Expected: 23 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: watermark-guarded chunked retention pruner"`.

---

### Task 7: Weekly export (CSV/JSON)

**Files:**
- Create: `crates/pcw-export/src/lib.rs`
- Test: `crates/pcw-export/tests/export.rs`

**Interfaces:**
- Consumes: `Store::open_readonly`, `for_each_connection_after`, `for_each_session_after`, `for_each_title_after`, `overview`, `get_meta`; `SharedStore` (writer) for `set_meta`.
- Produces: `pub struct ExportReport { pub folder: PathBuf, pub connection_rows: u64, pub session_rows: u64, pub title_rows: u64 }`; `pub enum ExportError { FolderMissing(PathBuf), Io(std::io::Error), Storage(StorageError), Csv(csv::Error), Json(serde_json::Error) }`; `pub fn run_export(db_path: &Path, writer: &SharedStore, export_folder: &Path, now_ms: u64) -> Result<ExportReport, ExportError>`.
- Behaviour (spec §7, §4.7):
  - If `export_folder` does not exist: create it only if its parent exists, else `FolderMissing`.
  - Reads through its own `Store::open_readonly(db_path)`; watermarks from `meta` (absent ⇒ 0, so the first export ships full history).
  - Folder name `YYYY-MM-DD` from `now_ms` in local time (`time::UtcOffset::current_local_offset()`, falling back to UTC); if taken, `-2`, `-3`, …
  - Writes into `<name>.partial/`, then renames to `<name>/`.
  - `connections.csv` columns `ts_first, ts_last, process_name, process_path, remote_ip, remote_port, local_port, protocol, direction, hostname`; `app-usage.csv` `ts_start, ts_end, duration_secs, process_name, window_title`; `window-titles.csv` `ts, process_name, title`. Timestamps RFC 3339 local. `csv::Writer` (RFC 4180 quoting). Rows streamed through the `for_each_*` callbacks.
  - `summary.json`: `{"total_connections", "top_apps", "top_services", "range": {"from_ms", "to_ms"}}` (from `overview` over the exported time range).
  - `manifest.json`: `{"connection_rows", "session_rows", "title_rows", "from_ms", "to_ms", "generated_at_ms", "schema_version", "tool_version": env!("CARGO_PKG_VERSION"), "counters": {<the six COUNTER_KEYS from meta, missing ⇒ 0>}}`.
  - Only after the rename: lock `writer`, set `export_wm_connections`, `export_wm_sessions`, `export_wm_titles` to the highest ids exported (never lower than before) and `last_export = now_ms`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn hostile_window_title_survives_csv_roundtrip() {
    // Review Focus #4: title = "He said \"hi\",\ta\nnext line" in a session AND a title row
    // export → read app-usage.csv and window-titles.csv back with csv::Reader → byte-identical
}
#[test] fn manifest_carries_counts_counters_and_version() { /* schema_version "1", tool_version == env!, counters has 6 keys, udp_capped == seeded 7 */ }
#[test] fn rows_written_after_export_go_in_next_export() {
    // Review Focus #1: export; then insert a row with ts_first OLDER than the first export's now_ms;
    // export again → second folder's connections.csv contains exactly that row; first folder untouched
}
#[test] fn watermarks_and_last_export_set_after_success() { /* get_meta values == max ids and now_ms */ }
#[test] fn missing_export_folder_fails_and_keeps_watermarks() { /* export_folder = <tmp>/nope/deeper → Err(FolderMissing); no watermark meta */ }
#[test] fn summary_json_keys_exact() { /* top-level keys exactly total_connections, top_apps, top_services, range */ }
#[test] fn second_export_same_day_gets_suffixed_folder() { /* two exports with same now_ms date → "<date>" and "<date>-2" */ }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-export` — Expected: FAIL.

- [ ] **Step 3: Implement `run_export`** per the Interfaces block.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-export` — Expected: 7 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: streaming incremental csv/json export with manifest"`.

---

### Task 8: Reverse-DNS fallback worker

**Files:**
- Create: `crates/pcw-resolve/src/lib.rs`
- Test: `crates/pcw-resolve/tests/resolve.rs`

**Interfaces:**
- Consumes: `SharedStore`, `Store::{pending_hostnames, set_ptr_result}`.
- Produces:
  - `pub trait NameResolver { fn lookup(&self, ip: &str) -> Option<String>; }`
  - `pub struct SystemResolver;` — `dns_lookup::lookup_addr(&ip.parse()?)`; any `Err` ⇒ `None` (it passes `NI_NAMEREQD`, so an IP with no PTR record is an error, not the IP echoed back).
  - `pub enum ResolveError { Storage(StorageError) }`
  - `pub fn resolve_pending(db: &SharedStore, resolver: &impl NameResolver, limit: u32, now_ms: u64) -> Result<usize, ResolveError>` — locks `db` only to read the list and to write each result, never during a lookup; returns the number named.
  - `pub fn run_worker(db: SharedStore, stop: Arc<AtomicBool>, interval: Duration, limit: u32)` — loops `resolve_pending` with `SystemResolver` until `stop`. Agent uses `interval = 30 s`, `limit = 50`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn resolved_names_are_stored_as_ptr() { /* fake maps 1.1.1.1 → "one.one.one.one"; row source 'ptr', hostname set */ }
#[test] fn failed_lookup_is_not_retried_within_24h_but_is_after() {
    // fake returns None and counts calls; run at now → 1 call, source 'none'; run at now+1h → still 1 call; at now+24h → 2
}
#[test] fn dns_names_are_never_overwritten_by_ptr() { /* Dns op names ip; worker run → hostname unchanged, source 'dns', 0 calls */ }
#[test] fn unnameable_ips_do_not_starve_new_ones() {
    // 200 pending ips that fail + 1 that resolves; limit 50, same now → within 5 rounds every ip has been tried once and the good one is 'ptr'
}
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-resolve` — Expected: FAIL.

- [ ] **Step 3: Implement** the trait, `SystemResolver`, `resolve_pending`, `run_worker`.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-resolve` — Expected: 4 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: reverse-dns fallback worker with negative cache"`.

---

### Task 9: Foreground session state machine

**Files:**
- Create: `crates/pcw-foreground/src/lib.rs`, `crates/pcw-foreground/src/tracker.rs` (pure logic; no `windows` imports)
- Test: `crates/pcw-foreground/tests/tracker.rs`

**Interfaces:**
- Consumes: `pcw_types::{AppSession, WindowTitle}`.
- Produces:
  - `pub enum SessionSignal { Foreground { ts_ms: u64, process_name: String, window_title: String }, Title { ts_ms: u64, process_name: String, window_title: String }, Locked { ts_ms: u64 }, Unlocked { ts_ms: u64 }, Suspended { ts_ms: u64 }, Resumed { ts_ms: u64 }, DisplayOff { ts_ms: u64 }, DisplayOn { ts_ms: u64 }, Paused { ts_ms: u64 }, Unpaused { ts_ms: u64 } }`
  - `pub enum TrackerOutput { Session(AppSession), Title(WindowTitle) }`
  - `pub struct OpenSession { pub ts_start_ms: u64, pub process_name: String, pub window_title: String }` (`Serialize, Deserialize, Clone, Debug, PartialEq`)
  - `pub struct SessionTracker` with `new(capture_titles: bool) -> Self`, `on_signal(&mut self, s: SessionSignal) -> Vec<TrackerOutput>`, `finalize(&mut self, ts_ms: u64) -> Vec<TrackerOutput>`, `open_session(&self) -> Option<OpenSession>`.
  - `pub const LOCK_APP: &str = "LockApp.exe"; pub const TITLE_MIN_MS: u64 = 2_000;`
- Rules (spec §4.3):
  - State: current foreground `(process, title)` (always tracked), flags `locked`, `suspended`, `display_off`, `paused`; *active* = no flag set.
  - While active and the current process is not `LOCK_APP`, exactly one session is open.
  - `Foreground` to a different process: close the open session at `ts_ms`, open a new one (title at open = its title). To the same process: treated as `Title`.
  - A flag setting closes the open session at `ts_ms`. The last flag clearing opens a session for the current foreground at `ts_ms` (unless it is `LOCK_APP` or unknown).
  - Title log: a pending `(ts, process, title)`; when the next title arrives or the session closes at `t`, the pending title is emitted as `Title` if `t - pending.ts >= TITLE_MIN_MS` and it differs from the last emitted title. An identical new title does not replace the pending one. No title logging while inactive, or when `capture_titles` is false or the title is empty.
  - `finalize(ts)` closes the open session (applying the title rule) at `ts`.

- [ ] **Step 1: Write the failing tests**

Helpers `fg(ts, app, title)`, `sessions(outputs)`, `titles(outputs)`.

```rust
#[test] fn app_switch_ends_and_starts() { /* fg a@0, fg b@100 → Session a [0,100]; open_session == b@100 */ }
#[test] fn lock_closes_and_idle_does_not() {
    // Review Focus #2: fg a@0, Locked@100 → Session a [0,100]; Unlocked@200 → reopens a@200;
    // no signals for 3_600_000 ms then finalize(3_600_200) → Session a [200, 3_600_200] (idle never ended it)
}
#[test] fn sleep_display_off_and_pause_each_close() { /* each of Suspended / DisplayOff / Paused closes the open session at its ts */ }
#[test] fn waking_or_display_on_while_locked_opens_nothing() {
    // Review Focus #2: Locked@100, Suspended@200, Resumed@300, DisplayOn@400 → no session open; Unlocked@500 → opens at 500
}
#[test] fn lock_app_never_gets_a_session() { /* fg a@0, Locked@50, fg LockApp.exe@60, Unlocked@90, fg a@95 → sessions: a[0,50], and a open from 95; none for LockApp */ }
#[test] fn title_changes_logged_after_2s_and_deduped() {
    // fg a "T1"@0, Title "T2"@1000 (T1 lasted 1 s → not logged), Title "T3"@5000 (T2 lasted 4 s → logged @1000),
    // Title "T3"@6000 (identical → ignored), finalize@9000 → T3 logged @5000; titles == [T2@1000, T3@5000]
}
#[test] fn same_process_foreground_change_is_title_not_session() { /* fg a@0, fg a "other"@3000, finalize@9000 → exactly 1 session */ }
#[test] fn titles_disabled_logs_nothing() { /* new(false) → no Title outputs, sessions still produced */ }
#[test] fn finalize_closes_open_session_at_given_ts() { /* fg a@10, finalize(70) → Session a [10,70]; open_session None */ }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-foreground` — Expected: FAIL.

- [ ] **Step 3: Implement `SessionTracker`** per the rules above.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-foreground` — Expected: 9 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: foreground session tracker with lock/sleep/display/pause state and title log"`.

---

### Task 10: Foreground OS adapter and Win32 helpers

**Files:**
- Create: `crates/pcw-foreground/src/os.rs`, `crates/pcw-foreground/src/sys.rs`
- Test: `crates/pcw-foreground/tests/os_smoke.rs` (one ignored manual test), `crates/pcw-foreground/tests/sys.rs`

**Interfaces:**
- Consumes: `SessionSignal` (Task 9), `pcw_types::SharedProcessCache`.
- Produces:
  - `os::start_hooks(tx: Sender<SessionSignal>, cache: SharedProcessCache, capture_titles: bool) -> Result<HookHandle, OsError>` — spawns one thread that: registers a window class and creates a hidden **top-level** window (`CreateWindowExW`, not `HWND_MESSAGE`); calls `WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION)` and `RegisterPowerSettingNotification(hwnd, &GUID_CONSOLE_DISPLAY_STATE, DEVICE_NOTIFY_WINDOW_HANDLE)`; sets `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, …, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS)`; sends a `Foreground` signal for the current `GetForegroundWindow()`; runs `GetMessageW` / `DispatchMessageW`. On each foreground change it unhooks the previous `EVENT_OBJECT_NAMECHANGE` hook and registers a new one for the foreground process id only; name-change callbacks are ignored unless `hwnd == GetForegroundWindow()`, `idObject == OBJID_WINDOW` and `idChild == CHILDID_SELF`. Window procedure maps: `WM_WTSSESSION_CHANGE` `WTS_SESSION_LOCK`/`WTS_SESSION_UNLOCK` → `Locked`/`Unlocked`; `WM_POWERBROADCAST` `PBT_APMSUSPEND` → `Suspended`, `PBT_APMRESUMEAUTOMATIC` → `Resumed`, `PBT_POWERSETTINGCHANGE` with `POWERBROADCAST_SETTING` data 0 → `DisplayOff`, 1 or 2 → `DisplayOn`. Process name from `GetWindowThreadProcessId` + the shared cache; title from `GetWindowTextW` (empty when `capture_titles` is false).
  - `HookHandle::stop(self)` — `PostThreadMessageW(thread_id, WM_QUIT, …)`, unhooks, joins.
  - `pub enum OsError { Win32(windows::core::Error), Thread(String) }`
  - `sys::attach_parent_console() -> bool` (`AttachConsole(ATTACH_PARENT_PROCESS)`); `sys::is_elevated() -> bool` (`OpenProcessToken` + `GetTokenInformation(TokenElevation)`); `sys::free_disk_bytes(path: &Path) -> Option<u64>` (`GetDiskFreeSpaceExW`); `sys::allow_taskbar_created_message()` (`ChangeWindowMessageFilter(RegisterWindowMessageW("TaskbarCreated"), MSGFLT_ADD)`).
  - Symbol modules (verified): `SetWinEventHook`/`UnhookWinEvent` in `Win32::UI::Accessibility`; `RegisterPowerSettingNotification`, `POWERBROADCAST_SETTING` in `Win32::System::Power`; `WTSRegisterSessionNotification`, `NOTIFY_FOR_THIS_SESSION` in `Win32::System::RemoteDesktop`; `GUID_CONSOLE_DISPLAY_STATE` in `Win32::System::SystemServices`; `GetModuleHandleW` in `Win32::System::LibraryLoader`; window, message, `WTS_SESSION_*`, `PBT_*`, `ChangeWindowMessageFilter` in `Win32::UI::WindowsAndMessaging`.

- [ ] **Step 1: Write the failing `sys` test**

```rust
#[test] fn free_disk_bytes_on_temp_dir_is_positive() { assert!(sys::free_disk_bytes(&std::env::temp_dir()).unwrap() > 0); }
```
Run: `cargo test -p pcw-foreground --test sys` — Expected: FAIL.

- [ ] **Step 2: Implement `sys.rs`** — Run: `cargo test -p pcw-foreground --test sys` — Expected: 1 passed.

- [ ] **Step 3: Implement `os.rs`** per the Interfaces block.

- [ ] **Step 4: Write the ignored manual harness** — `#[test] #[ignore = "manual: needs interactive session"]` starts the hooks with a fresh cache and prints signals for 90 s.

Run: `cargo test -p pcw-foreground --test os_smoke -- --ignored --nocapture`
Expected: alt-tab → `Foreground` with process and title; switching browser tabs → `Title`; Win+L → `Locked`, then `Unlocked` after sign-in; display off (`powercfg` short timeout or closing the lid with "do nothing") → `DisplayOff`/`DisplayOn`; no signals while idle at the desktop.

- [ ] **Step 5: Add the smoke checklist to README** — the expectations from Step 4.

- [ ] **Step 6: Commit** — `git commit -m "feat: win32 foreground, title, lock, power and display hooks; agent win32 helpers"`.

---

### Task 11: ETW network source with watchdog

**Files:**
- Create: `crates/pcw-etw/src/lib.rs`, `src/decode.rs`, `src/process.rs`, `src/snapshot.rs`, `src/source.rs`, `src/supervisor.rs`, `src/rate.rs`, `examples/capture.rs`
- Test: `crates/pcw-etw/tests/decode.rs`, `tests/supervisor.rs`, `tests/rate.rs`

**Interfaces:**
- Consumes: `pcw_types::{ConnectionEvent, DnsAnswer, EventKind, Protocol, Direction, SharedProcessCache, Counters}`.
- Produces:
  - `pub enum NetEvent { Connection(ConnectionEvent), Dns(DnsAnswer) }`; `pub const SESSION_NAME: &str = "PC-Watcher-Net";`
  - Provider constants (spec §4.1): `KERNEL_NETWORK = "7dd42a49-5329-4832-8dfd-43d979153a88"` (keywords `0x30`, event IDs `[12, 13, 15, 28, 29, 31, 42, 43, 58, 59]`), `DNS_CLIENT = "1c95126e-7eea-49a9-a3fe-a378b03ddb4d"` (keywords left at ferrisetw's default `0` = all; event IDs `[3008]`), `KERNEL_PROCESS = "22fb2cd6-0e7b-422b-a0c7-2fad1fd0e716"` (keywords `0x10`, event IDs `[1, 2]`). Each provider gets `.add_filter(EventFilter::ByEventIds(ids))`.
  - `decode::port_from_network_order(raw: u16) -> u16` — `u16::from_be(raw)`. Needed because ferrisetw parses `u16` with `from_ne_bytes` but these ports are `win:Port`; the same applies to `GetExtendedTcpTable` port fields.
  - `decode::kernel_network_kind(event_id: u16) -> Option<(Protocol, Option<Direction>, EventKind)>` — 12, 28 → `(Tcp, Some(Out), Open)`; 15, 31 → `(Tcp, Some(In), Open)`; 13, 29 → `(Tcp, None, Close)`; 42, 58 → `(Udp, Some(Out), Packet)`; 43, 59 → `(Udp, Some(In), Packet)`; else `None`.
  - `decode::guess_direction(local_port: u16) -> Direction` — 49152..=65535 ⇒ `Out`, else `In` (only for unmatched closes).
  - `decode::parse_dns_results(results: &str) -> Vec<String>` — split on `;`, trim, skip empty and entries starting with `type:`, strip a leading `::ffff:`, keep entries that parse as `IpAddr`.
  - `decode::snapshot_direction(local_port: u16, listening: &HashSet<u16>) -> Direction` — in `listening` ⇒ `In`, else `Out`.
  - Event fields (verified in this PC's manifests): Kernel-Network `PID` (u32), `daddr`/`saddr` (`IpAddr`), `dport`/`sport` (u16, network order); DNS-Client 3008 `QueryName`, `QueryStatus` (u32), `QueryResults`; Kernel-Process 1/2 `ProcessID`, `ImageName`. Remote = `daddr`/`dport`, local port = `sport`.
  - `process::query_process(pid: u32) -> Option<(String, String)>` — the only Win32 process lookup (`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageNameW`), so `pcw-agent` builds the one shared `ProcessCache` without importing `windows`.
  - `snapshot::tcp_snapshot_events(cache: &SharedProcessCache, ts_ms: u64) -> Vec<ConnectionEvent>` — `GetExtendedTcpTable` for `AF_INET` and `AF_INET6` with `TCP_TABLE_OWNER_PID_ALL`; listening ports from `MIB_TCP_STATE_LISTEN` rows; one `Open` event per established row.
  - `source::SourceConfig { pub capture_dns: bool, pub udp_rate_cap: u32 }`; `source::run_session(cfg: &SourceConfig, cache: SharedProcessCache, counters: Arc<Counters>, paused: Arc<AtomicBool>, sink: Sender<NetEvent>) -> Result<(), SourceError>` — builds the three providers, `UserTrace::new().named(SESSION_NAME.into()).set_trace_properties(TraceProperties { buffer_size: 64, ..Default::default() })`, `.start()`, then blocks in `UserTrace::process_from_handle(handle)` until the session ends. Callbacks: Kernel-Process start/stop → `cache.note_process(pid, image, started)`; Kernel-Network → `ConnectionEvent` (name/path from the cache; `Close` direction from `guess_direction`); UDP passes `RateLimiter::allow` first; DNS 3008 with `QueryStatus == 0` → `NetEvent::Dns` (skipped when `!capture_dns`). Nothing is sent while `paused` is set.
  - `source::query_loss(session: &str) -> Option<(u64, u64)>` — `ControlTraceW(CONTROLTRACE_HANDLE::default(), name, props, EVENT_TRACE_CONTROL_QUERY)` → `(EventsLost, RealTimeBuffersLost)`; buffer sized `size_of::<EVENT_TRACE_PROPERTIES>() + 1024` with `LoggerNameOffset` set.
  - `source::stop_session(session: &str)` — `ferrisetw::trace::stop_trace_by_name`, error ignored.
  - `pub enum SourceError { Trace(String), Win32(String) }`
  - `supervisor::Supervisor { pub restarts: u64, pub events_lost: u64, pub buffers_lost: u64, /* private: backoff, per-session baseline */ }` with `new() -> Self`, `on_exit(&mut self, result: Result<(), SourceError>, ran_for: Duration) -> Duration` (counts a restart; returns the wait: 1 s doubling to a 60 s cap, back to 1 s if `ran_for >= 5 min`; resets the loss baseline) and `on_stats(&mut self, session_events_lost: u64, session_buffers_lost: u64)` (adds the increase since the last call in this session).
  - `rate::RateLimiter` with `new(cap_per_sec: u32) -> Self`, `allow(&mut self, pid: u32, ip: IpAddr, port: u16, now_ms: u64) -> bool`, `pub capped: u64`. `cap == 0` ⇒ always `true`, no map. Keys idle for 10 s are dropped, checked once a minute.

- [ ] **Step 1: Write the failing decode tests**

```rust
#[test] fn ports_are_converted_from_network_order() { assert_eq!(port_from_network_order(0xBB01), 443); }
#[test] fn kernel_network_ids_map_to_kinds() {
    assert_eq!(kernel_network_kind(12), Some((Protocol::Tcp, Some(Direction::Out), EventKind::Open)));
    assert_eq!(kernel_network_kind(31), Some((Protocol::Tcp, Some(Direction::In), EventKind::Open)));
    assert_eq!(kernel_network_kind(29), Some((Protocol::Tcp, None, EventKind::Close)));
    assert_eq!(kernel_network_kind(59), Some((Protocol::Udp, Some(Direction::In), EventKind::Packet)));
    assert_eq!(kernel_network_kind(10), None); // data events never used
}
#[test] fn dns_results_parse_ips_and_skip_cnames() {
    assert_eq!(parse_dns_results("type:  5 github.com;::ffff:140.82.112.3;"), vec!["140.82.112.3"]);
    assert_eq!(parse_dns_results("2606:4700::6810:84e5;104.16.132.229;"), vec!["2606:4700::6810:84e5", "104.16.132.229"]);
    assert!(parse_dns_results("").is_empty());
}
#[test] fn snapshot_and_guessed_directions() { /* 443 in listening → In; 50000 not → Out; guess_direction(51515) Out, guess_direction(443) In */ }
```

- [ ] **Step 2: Write the failing supervisor and rate tests**

```rust
#[test] fn restarts_with_doubling_backoff_capped_at_60s() { /* five quick Err exits → 1,2,4,8,16 s; ten more → capped 60 s; restarts == 15 */ }
#[test] fn backoff_resets_after_a_long_run() { /* after waits reach 8 s, on_exit(Err, 6 min) → 1 s */ }
#[test] fn clean_exit_still_restarts() { /* on_exit(Ok(()), 1 s) → restart counted, wait 1 s */ }
#[test] fn loss_counts_increase_only() { /* on_stats(5,1), on_stats(8,1) → events_lost 8; after on_exit, on_stats(2,0) → 10 */ }
#[test] fn cap_zero_allows_everything() { /* 1000 same-key allows in one ms → all true, capped 0 */ }
#[test] fn cap_five_per_second() { /* cap 5: 5 true then false at same second; capped == 1; after +1000 ms true again */ }
```

- [ ] **Step 3: Run tests to verify they fail** — Run: `cargo test -p pcw-etw` — Expected: FAIL.

- [ ] **Step 4: Implement `decode`, `supervisor`, `rate`** — Run: `cargo test -p pcw-etw` — Expected: 10 passed.

- [ ] **Step 5: Implement `process`, `snapshot`, `source` and `examples/capture.rs`** — the example stops a leftover session, prints the snapshot, runs `run_session` on a thread for 20 s, prints every `NetEvent`, prints `query_loss` at the end, then `stop_session`.

- [ ] **Step 6: Manual ETW smoke (elevated PowerShell)**

Run: `cargo run -p pcw-etw --example capture` while browsing.
Expected: (a) TCP `Open` rows with real process names (`msedge.exe`, `chrome.exe`, …) and remote port **443** (not 47873 — that would mean a missed byte swap); (b) `Close` events follow; (c) `Dns` events with names like `www.bing.com` and their IPs; (d) for receive events (43/59) and accepts (15/31) the `remote_ip` is the other machine, not this PC's address — if reversed, swap `saddr`/`daddr` for those IDs in `decode` and add a test; (e) `query_loss` returns `Some`.

- [ ] **Step 7: Watchdog and volume smoke**

(a) While the example runs, run `logman stop PC-Watcher-Net -ets` in another elevated shell → `run_session` returns and the example prints that it did (the full restart loop is checked in Task 13/15). (b) Hibernate and resume while the example runs → note whether events keep arriving or `run_session` returns; record it in README. (c) Run a 10-minute video call: record events/s for UDP, CPU of the example process, and estimated DB growth (rows × 200 bytes) in README "Measured volume" (spec §4.6, §12).

- [ ] **Step 8: Commit** — `git commit -m "feat: etw kernel-network/dns/process source with watchdog, snapshot, udp cap"`.

---

### Task 12: Agent core — writer, timers, counters, crash recovery, log

**Files:**
- Create: `crates/pcw-agent/src/lib.rs`, `src/writer.rs`, `src/state.rs`, `src/timers.rs`, `src/logfile.rs`
- Test: `crates/pcw-agent/tests/core.rs`

**Interfaces:**
- Consumes: `Store::{apply_batch, set_meta, get_meta, delete_meta}`, `WriteOp`, `StorageError::DiskFull`, `pcw_types::{Counters, COUNTER_KEYS, AppSession}`, `pcw_foreground::OpenSession`.
- Produces:
  - `pub enum WriterMsg { Op(WriteOp), OpenSession { session: Option<OpenSession>, now_ms: u64 }, Shutdown }`
  - `pub struct WriterConfig { pub min_free_bytes: u64, pub free_bytes: Box<dyn Fn() -> Option<u64> + Send>, pub disk_check_every: Duration }`
  - `writer::run_writer(rx: Receiver<WriterMsg>, db: SharedStore, cfg: WriterConfig, counters: Arc<Counters>)` — flushes with `apply_batch` every 1 s **or** at 500 queued ops, whichever first. Every `disk_check_every` (60 s in the agent) it calls `free_bytes`; while below `min_free_bytes`, UDP `Packet` ops are dropped and counted in `udp_skipped_low_disk`. `DiskFull` ⇒ batch dropped, its op count added to `rows_dropped_disk_full`, writer continues. `OpenSession` ⇒ `state::save_open_session`. `Shutdown` ⇒ flush what is queued and return.
  - `state::save_open_session(store: &mut Store, s: Option<&OpenSession>, now_ms: u64) -> Result<(), StorageError>` — `meta.open_session` = JSON `{ts_start_ms, process_name, window_title, saved_at_ms}`, or deleted when `None`.
  - `state::recover_open_session(store: &mut Store) -> Result<Option<AppSession>, StorageError>` — if present, inserts the session `[ts_start_ms, saved_at_ms]`, deletes the key, returns it.
  - `state::persist_counters(store: &mut Store, c: &Counters)`, `state::load_counters(store: &Store, c: &Counters)` — the six `COUNTER_KEYS` as decimal strings.
  - `timers::due(now_ms: u64, last_ms: Option<u64>, period_ms: u64) -> bool` — `None` ⇒ true.
  - `logfile::init(path: &Path, max_bytes: u64)` — a `log::Log` writing timestamped lines; when the file exceeds `max_bytes` (5 MB in the agent) it is renamed to `agent.log.1` (replacing the old one).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn writer_flushes_within_1_5s_and_drains_on_shutdown() { /* 3 Connection ops → visible in the DB within 1.5 s; 2 more then Shutdown → thread joins, 5 rows */ }
#[test] fn writer_drops_udp_when_disk_low_and_counts() {
    // Review Focus #5: free_bytes returns Some(1), min_free_bytes 2 GB → 4 UDP ops + 1 TCP Open → 1 row; udp_skipped_low_disk == 4
}
#[test] fn writer_survives_disk_full() { /* store with tiny max_page_count; 5,000 UDP ops → rows_dropped_disk_full > 0; raise limit; later TCP op is written */ }
#[test] fn open_session_recovery_writes_session_and_clears_meta() { /* save Some(a@1000) at 61_000; recover → AppSession [1000, 61_000]; meta key gone; second recover → None */ }
#[test] fn counters_persist_and_reload() { /* add udp_capped 7, persist, fresh Counters, load → 7 */ }
#[test] fn due_logic() { assert!(due(10, None, 5)); assert!(!due(10, Some(8), 5)); assert!(due(13, Some(8), 5)); }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-agent` — Expected: FAIL.

- [ ] **Step 3: Implement `writer`, `state`, `timers`, `logfile`.**

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-agent` — Expected: 6 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: agent writer with disk guards, crash recovery, counters, log file"`.

---

### Task 13: Agent binary — install, tray, wiring

**Files:**
- Create: `crates/pcw-agent/src/install.rs`, `src/tray.rs`; Modify: `crates/pcw-agent/src/main.rs`, `src/lib.rs`
- Test: `crates/pcw-agent/tests/install.rs`

**Interfaces:**
- Consumes: Tasks 2–12 — `Config::load_or_create(default_data_dir()?)`, `Store::open(data_dir/data.db)`, `run_writer`, `recover_open_session`, `load_counters`/`persist_counters`, `SessionTracker` + `os::start_hooks` + `sys::*`, `run_session`/`query_loss`/`stop_session`/`tcp_snapshot_events`/`Supervisor`/`query_process`, `run_worker`, `run_export`, `Store::prune_step`.
- Produces:
  - `install::task_xml(exe: &Path, user: &str) -> String` (pure) — the template below with `{user}` and `{exe}` XML-escaped (`&`, `<`, `>`, `"`).
  - `install::utf16_le_bom(s: &str) -> Vec<u8>` — `FF FE` + UTF-16 LE.
  - `install::install() -> Result<(), String>` — not `sys::is_elevated()` ⇒ `Err("run from an elevated terminal")`; `schtasks /End /TN PC-Watcher` (ignore failure); copy `pcw-agent.exe` and sibling `pcw-dash.exe` to `<data_dir>\bin\`; write the XML (UTF-16 LE BOM) to a temp file; `schtasks /Create /TN PC-Watcher /XML <file> /F`; `schtasks /Run /TN PC-Watcher`. Non-zero exit ⇒ `Err(stderr)`. `user` = `%USERDOMAIN%\%USERNAME%`.
  - `install::uninstall() -> Result<(), String>` — `/End` then `/Delete /TN PC-Watcher /F`; data stays.
  - `main()`: `--install` / `--uninstall` ⇒ `sys::attach_parent_console()`, print result, exit 0/1. Otherwise run the agent.
  - Threads: **ETW** (loop: `stop_session`, send `tcp_snapshot_events`, `run_session`, then `Supervisor::on_exit` + sleep, until exiting; `Supervisor` behind a `Mutex` shared with the timer thread); **hooks** (`start_hooks` → `Sender<SessionSignal>`); **tracker** (owns `SessionTracker`, `recv_timeout(60 s)`, sends outputs as `WriterMsg::Op` and every 60 s `WriterMsg::OpenSession`); **writer**; **PTR worker** (only if `resolve_dns`); **timers** (every 60 s: `query_loss` → `on_stats`, copy supervisor numbers into `Counters`, `persist_counters`; hourly: `prune_step(…, 50_000)` in a loop releasing the lock between calls; export when `last_export` is absent or ≥ 7 days old, retried hourly after a failure); **main** = tray. NetEvent → `WriteOp::Connection`/`WriteOp::Dns`.
  - Tray (`tray-icon` + `tao` event loop on the main thread): items exactly **Open Dashboard** (spawn `<exe dir>\pcw-dash.exe`), **Pause / Resume** (toggle the shared `paused` flag and send `Paused`/`Unpaused` to the tracker), **Export now** (`run_export` on a thread), **Open data folder** (`explorer.exe <data_dir>`), **Exit** (`stop_session`, stop hooks, `finalize` the tracker, `Shutdown` the writer, join). Tooltip refreshed every 60 s: `PC-Watcher — collecting|paused · last export <date|never>` plus `· export overdue` (> 8 days) / `· low disk`. Call `sys::allow_taskbar_created_message()` before creating the icon.

Task XML template (exact copy; spec §6):

```xml
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>PC-Watcher agent</Description></RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
    <RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec><Command>{exe}</Command></Exec>
  </Actions>
</Task>
```

- [ ] **Step 1: Write the failing install tests**

```rust
#[test] fn task_xml_disables_battery_and_time_limits() {
    let x = task_xml(Path::new("C:\\Users\\K & Co\\pcw-agent.exe"), "PC\\Kartik");
    for s in ["<DisallowStartIfOnBatteries>false", "<StopIfGoingOnBatteries>false", "<ExecutionTimeLimit>PT0S",
              "<RunLevel>HighestAvailable", "<LogonType>InteractiveToken", "<Priority>5", "<MultipleInstancesPolicy>IgnoreNew"] {
        assert!(x.contains(s), "{s}");
    }
    assert!(x.contains("K &amp; Co")); assert!(!x.contains("K & Co"));
}
#[test] fn utf16_file_has_bom() { let b = utf16_le_bom("<a/>"); assert_eq!(&b[..2], &[0xFF, 0xFE]); assert_eq!(b.len(), 2 + 4 * 2); }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-agent --test install` — Expected: FAIL.

- [ ] **Step 3: Implement `install.rs`** — Run: `cargo test -p pcw-agent` — Expected: 8 passed.

- [ ] **Step 4: Implement `main.rs` wiring and `tray.rs`** per the Interfaces block.

- [ ] **Step 5: Manual smoke — install, battery, footprint** (spec §6, §12)

Run (elevated): `cargo build --release; .\target\release\pcw-agent.exe --install` (use `$env:CARGO_TARGET_DIR\release\` if set).
Checklist: (a) prints success; `schtasks /Query /TN PC-Watcher /XML` shows `PT0S`, both battery settings `false`, `HighestAvailable`, command under `%LOCALAPPDATA%\PC-Watcher\bin\`; (b) sign out and in → agent starts with **no UAC prompt** and **no console window**; (c) tray icon with exactly the five items; (d) unplug the charger → agent keeps running; (e) restart Explorer (`Stop-Process -Name explorer`) → tray icon comes back; (f) Task Manager after 10 min idle: memory **< 40 MB**, CPU average **< 1%**; (g) Pause → `connections` and `app_sessions` stop growing; Resume → they grow again; (h) Export now → dated folder in `export_folder` — **if the elevated agent cannot see `G:`, record it and switch `export_folder` to the local-folder fallback in spec §7**; (i) `agent.log` has start-up lines; (j) `--uninstall` removes the task; data folder remains.

- [ ] **Step 6: Commit** — `git commit -m "feat: agent binary with xml task install, tray, thread wiring"`.

---

### Task 14: Dashboard server and embedded UI

**Files:**
- Create: `crates/pcw-dashboard/src/lib.rs`, `src/routes.rs`, `src/ui.html`
- Test: `crates/pcw-dashboard/tests/server.rs`

**Interfaces:**
- Consumes: `Store::open_readonly` and the Task 5 queries, `ConnFilter`.
- Produces:
  - `pub struct DashServer`; `DashServer::bind(port: u16) -> Result<DashServer, DashError>` — `tiny_http::Server::http(("127.0.0.1", port))`, **never** `0.0.0.0`/`[::]`; address in use ⇒ `DashError::PortInUse`. `local_addr(&self) -> SocketAddr` (from `server_addr().to_ip()`). `run(self, db_path: PathBuf, stop: Arc<AtomicBool>, idle_timeout: Duration) -> Result<(), DashError>` — opens the read-only store lazily and retries on each API request until it succeeds; returns when `stop` is set, after `POST /api/shutdown`, or after `idle_timeout` with no requests.
  - `pub enum DashError { PortInUse, Bind(String), Storage(StorageError) }`
  - `routes::parse_filter(query: &str) -> Result<ConnFilter, String>` — params `from`, `to` (ms), `app`, `protocol` (`tcp|udp`), `port`, `direction` (`out|in`); bad value ⇒ `Err`.
  - Routes: `GET /` → `ui.html` (`include_str!`); `GET /api/overview?<filter>` → `OverviewSummary`; `GET /api/app?name=…&limit=…&<filter>` → `{"destinations", "connections", "usage", "titles"}`; `GET /api/service?key=…&limit=…&<filter>` → connections; `GET /api/usage?from&to&app` → `{"sessions", "titles"}`; `POST /api/shutdown` → 200 then stop. Bad parameter → 400; DB not available → 503 `{"error": "no data yet — is pcw-agent running?"}`; unknown route → 404. JSON via `serde_json`.
  - `ui.html`: one file, vanilla JS, four views (Overview, App drill-down, Service drill-down, Usage with the title log), filter bar (date range, app, protocol, port, direction), hourly SVG bar chart, Stop button calling `/api/shutdown`. The only URL-like string allowed is the SVG namespace `http://www.w3.org/2000/svg`.

- [ ] **Step 1: Write the failing tests**

Helpers: seed a temp DB with `Store::open`; spawn `run` on a thread; raw `std::net::TcpStream` HTTP/1.0 requests (no new dependencies).

```rust
#[test] fn binds_loopback_only() { /* Review Focus #3: bind(0) → local_addr().ip() == 127.0.0.1 */ }
#[test] fn overview_returns_seeded_json() { /* GET /api/overview → 200, body contains "a.exe" */ }
#[test] fn app_endpoint_has_drill_down_shape() { /* keys exactly destinations, connections, usage, titles */ }
#[test] fn bad_filter_is_400() { /* protocol=xyz → 400; parse_filter("protocol=udp&port=53&direction=in") → Ok with those values */ }
#[test] fn missing_db_is_503_then_recovers() { /* db_path absent → 503; create DB → next request 200 */ }
#[test] fn unknown_route_404s() { /* "404" in the status line */ }
#[test] fn shutdown_endpoint_stops_server() { /* POST /api/shutdown → 200; run() returns within 2 s */ }
#[test] fn ui_has_no_external_urls() {
    let ui = include_str!("../src/ui.html").replace("http://www.w3.org/2000/svg", "");
    assert!(!ui.contains("http://") && !ui.contains("https://") && !ui.contains("//cdn"));
}
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-dashboard` — Expected: FAIL.

- [ ] **Step 3: Implement `DashServer` and `routes.rs`.**

- [ ] **Step 4: Write `ui.html`** — views wired to the endpoints; clicking an app opens its drill-down; clicking a service shows the apps that talked to it; Overview defaults to `from` = local midnight today.

- [ ] **Step 5: Run tests to verify they pass** — Run: `cargo test -p pcw-dashboard` — Expected: 8 passed.

- [ ] **Step 6: Commit** — `git commit -m "feat: loopback dashboard server with filters and embedded offline ui"`.

---

### Task 15: `pcw-dash` binary and end-to-end verification

**Files:**
- Create: `crates/pcw-dash/src/main.rs`
- Modify: `README.md` (final: install, config reference, smoke script)

**Interfaces:**
- Consumes: `Config::load_or_create`, `default_data_dir`, `DashServer::{bind, run}`, `DashError::PortInUse`.
- Produces: `pcw-dash.exe [--port N]` (`#![windows_subsystem = "windows"]`) — port defaults to `config.dashboard_port` (7411); `bind`; on `PortInUse` open the browser to that port and exit 0 (a dashboard is already running); otherwise open `explorer.exe http://127.0.0.1:<port>/` (so an elevated parent never starts an elevated browser) and `run(data_dir/data.db, stop, 3600 s)`.

- [ ] **Step 1: Implement `pcw-dash` main** per the Interfaces block.

- [ ] **Step 2: End-to-end verification** (the spec's acceptance; record results in the commit message)

1. `pcw-agent --install` (elevated), sign out/in → no UAC prompt, no console window, tray present, idle footprint within spec §12.
2. Browse for 10 minutes → tray **Open Dashboard** → the browser opens as the normal user (Task Manager "Elevated" column = No); Overview shows the activity; click an app → destinations, connection rows with times, ports and names like `www.bing.com` (DNS source); click a service → the apps that talked to it; Usage shows sessions with the title log (tab switches appear).
3. Filters: protocol UDP, port 443, direction in — each changes the results.
4. Lock for 5 min mid-session → Usage excludes it; wake from sleep and leave it at the lock screen 2 min → no session for that time, no `LockApp.exe` session (Review Focus #2).
5. **Export now** → `connections.csv`, `app-usage.csv`, `window-titles.csv`, `summary.json`, `manifest.json` in the export folder; titles intact; counters present.
6. Kill `pcw-agent.exe` in Task Manager → Task Scheduler restarts it within ~1 min (`RestartOnFailure`); `agent.log` shows a leftover ETW session stopped and the open session recovered.
7. From another device on the LAN, `http://<lan-ip>:7411` is unreachable while the dashboard runs (Review Focus #3).
8. Click **Stop** in the dashboard → `pcw-dash.exe` exits.

- [ ] **Step 3: Final README** — install/uninstall, config reference (exact spec §8 keys and comments), smoke checklists from Tasks 10, 11, 13, measured volume from Task 11, `cargo build --timings` note, export-folder fallback (spec §7).

- [ ] **Step 4: Commit** — `git commit -m "feat: on-demand dashboard binary and e2e verification"`.

---

## Self-Review Notes (for the executor)

- Spec coverage: §4.1 → Task 11; §4.2 → Tasks 2, 11; §4.3 → Tasks 9, 10, 12; §4.4 → Tasks 4, 11; §4.5 → Tasks 4, 8, 11; §4.6 → Tasks 4, 5, 12; §4.7 → Tasks 6, 7, 12; §5 → Tasks 13–15; §6 → Task 13; §7 → Task 7; §8 → Task 3; §11 → Task 1; §12 → Tasks 11, 13; §13 → each task's tests.
- Type consistency: event types and `Counters` defined once (Task 2); `WriteOp`/`SharedStore` once (Task 4); query DTOs and `ConnFilter` once (Task 5); signals and `OpenSession` once (Task 9); `NetEvent` once (Task 11); `WriterMsg` once (Task 12).
- Test counts per crate: `pcw-types` 8; `pcw-config` 4; `pcw-storage` 10 → 18 → 23; `pcw-export` 7; `pcw-resolve` 4; `pcw-foreground` 9 + 1 (`sys`) + 1 ignored; `pcw-etw` 10; `pcw-agent` 6 → 8; `pcw-dashboard` 8.
- Windows-only tasks (10, 11, 13, 15) carry manual smoke checklists because automated tests cannot lock a screen, sleep the machine or unplug a charger — this is deliberate, not a gap.
