# PC-Watcher Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an always-on, lightweight Windows tray app that records per-process network connection metadata and foreground app usage into a local SQLite DB, with an on-demand localhost dashboard and weekly structured export to a Google Drive folder.

**Architecture:** A Rust cargo workspace of ten small crates producing two binaries: `pcw-agent` (elevated logon scheduled task; ETW subscription, WinEvent foreground hooks, batched SQLite writer, pruner, exporter, tray) and `pcw-dash` (on-demand read-only server on 127.0.0.1 serving one embedded HTML file). Raw rows — one per connection — with a write-time TCP lifecycle upsert; UDP is one row per datagram. All shared state flows through `SharedStore = Arc<Mutex<Store>>`.

**Tech Stack:** Rust stable, `windows` crate (only in `pcw-etw`/`pcw-foreground`), `etw` crate, `rusqlite` (bundled), `tiny_http`, `tray-icon` + `tao`, `dns-lookup`, `serde`/`serde_json`/`toml`/`csv`, `lld`/fast linker via `.cargo/config.toml`.

**Spec:** `docs/superpowers/specs/2026-10-21-pc-watcher-design.md` — the plan argues from the spec; executors read both.

## Global Constraints

- Platform: Windows 10/11 x64 only, Rust stable toolchain.
- Metadata only: never collect payloads, URLs, DNS query contents, HTTP headers, keystrokes, screenshots, file contents, or command lines. Window titles: **captured** (spec §4.3).
- Raw rows: one row per connection with its own timestamp; no merge/coalescing mode exists (spec §4.4). TCP rows upsert open→close lifecycle; UDP = one row per datagram.
- DB path: `%LOCALAPPDATA%\PC-Watcher\data.db`, env override `PCW_DATA_DIR` — never inside a sync root (OneDrive Desktop) (spec §4.6).
- Dashboard binds `127.0.0.1` only, default port `7411`; no external CDN/URL references in the embedded HTML (spec §5.2).
- Config file `%LOCALAPPDATA%\PC-Watcher\config.toml` with exact defaults: `retention_days = 10`, `resolve_dns = true`, `capture_window_titles = true`, `export_folder = "G:\\My Drive\\PC-Watcher"`, `export_schedule = "weekly"`, `dashboard_port = 7411`, `udp_rate_cap = 0` (spec §8).
- Pruner cutoff = `min(retention cutoff, meta.last_export)`; if `last_export` is absent, prune nothing (spec §4.7).
- Session end: app-switch, lock, sleep, hibernate, monitor-off; idle does NOT end a session (spec §4.3).
- `windows` crate appears only in `pcw-etw` and `pcw-foreground`, with minimal explicit features (spec §11.4).
- Root `[workspace.dependencies]` pins every dependency once; members use `dep = { workspace = true }` (spec §11.2).
- Release profile: `lto = "thin"`, `opt-level = 3`, `strip = true` (spec §11.6).
- Footprint targets (manual smoke): agent < 40 MB RAM, < 1% average CPU idle (spec §12).
- Agent install: `schtasks` logon task `/RL HIGHEST`, program name `PC-Watcher` (spec §6).
- Tray menu exactly: Open Dashboard, Pause / Resume, Export now, Open data folder, Exit (spec §5.1).
- Conventional commits (`feat:`, `fix:`, `test:`, `chore:`, `docs:`); every task ends with a commit.

## Review Focus

Failure modes the spec implies that most tasks don't exercise — each pinned to the task that owns it:

1. **Dashboard must read the WAL database while the agent is actively writing** — a user opening the dashboard mid-collection must not get "database is locked" or partial-crash. → Task 5 concurrent test.
2. **Pruner must never delete rows newer than the last successful export** (failed/skipped weekly export) — data loss is unrecoverable. → Task 6 tests.
3. **Dashboard must be unreachable from the network** — a mis-bound server would expose monitoring data to the LAN, violating spec anti-goal "no network exposure". → Task 13 bind assertion.
4. **Lock/sleep must close a foreground session; idle must not** — getting this wrong roughly doubles reported daily usage. → Task 9 state-machine tests.
5. **Window titles containing quotes, commas, tabs, and newlines must survive CSV export intact** — titles are user-controlled free text and land in Drive spreadsheets. → Task 7 golden test.

---

### Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml`, `.cargo/config.toml`, `.gitignore`, `README.md`
- Create: `crates/{pcw-types,pcw-config,pcw-storage,pcw-export,pcw-resolve,pcw-etw,pcw-foreground,pcw-dashboard}/Cargo.toml` + `src/lib.rs` stubs
- Create: `crates/pcw-agent/Cargo.toml` + `src/main.rs` stub, `crates/pcw-dash/Cargo.toml` + `src/main.rs` stub

**Interfaces:**
- Produces: the workspace all later tasks build inside. Package names are exactly the directory names; `pcw-agent` and `pcw-dash` are the only `[[bin]]` crates.

- [ ] **Step 1: Initialize the git repository**

Run: `git init`, create `.gitignore` containing `target/`, `*.db`, `*.db-wal`, `*.db-shm`.
Run: `git add -A && git commit -m "chore: init repo"` — Expected: commit created.

- [ ] **Step 2: Write the root `Cargo.toml`**

Workspace with `members = ["crates/*"]`, `[workspace.dependencies]` pinning exactly one version each of: `serde` (derive), `serde_json`, `toml`, `thiserror`, `rusqlite` (feature `bundled`), `csv`, `dns-lookup`, `etw`, `windows` (explicit minimal feature list: `Win32_Foundation`, `Win32_System_Threading`, `Win32_System_ProcessStatus`, `Win32_UI_Accessibility`, `Win32_UI_WindowsAndMessaging`, `Win32_System_Power`, `Win32_System_SessionNotification`), `tiny_http`, `tray-icon`, `tao`. Also `[profile.release] lto = "thin"`, `opt-level = 3`, `strip = true`, and `[profile.dev] incremental = true`.

- [ ] **Step 3: Write `.cargo/config.toml` with a fast linker**

Enable a fast linker for the host target (candidates in decreasing availability: `rust-lld`, an installed `lld-link`, or the toolchain's default if none resolves). Verify the choice works before committing — the check is Step 5's `cargo build -v`.

- [ ] **Step 4: Scaffold the ten member crates**

Eight `--lib` (`pcw-types`, `pcw-config`, `pcw-storage`, `pcw-export`, `pcw-resolve`, `pcw-etw`, `pcw-foreground`, `pcw-dashboard`) and two `--bin` (`pcw-agent`, `pcw-dash`) under `crates/`, each with a one-line `src/lib.rs`/`src/main.rs` stub. Root `README.md` documents build (`cargo build --release`), the `cargo --timings` profiling command (spec §11.7), and the chosen linker config.

- [ ] **Step 5: Build the workspace**

Run: `cargo build -v 2>&1 | Select-Object -First 40`
Expected: all ten crates compile; linker line visible in verbose output. Then `git add -A && git commit -m "chore: cargo workspace scaffold with fast linker"`.

---

### Task 2: Shared types and process cache

**Files:**
- Create: `crates/pcw-types/src/lib.rs` (module `process`)
- Test: `crates/pcw-types/tests/types.rs`

**Interfaces:**
- Consumes: nothing.
- Produces (later tasks depend on these exact names):
  - `pub enum Protocol { Tcp, Udp }`, `pub enum Direction { Out, In }` — both `serde::Serialize`, `Copy`, `PartialEq`.
  - `pub struct ConnectionEvent { pub ts_ms: u64, pub pid: u32, pub process_name: String, pub process_path: String, pub remote_ip: String, pub remote_port: u16, pub local_port: u16, pub protocol: Protocol, pub direction: Direction }`
  - `pub struct AppSession { pub ts_start_ms: u64, pub ts_end_ms: u64, pub process_name: String, pub window_title: String }`
  - `pub struct ProcessCache<R: FnMut(u32) -> Option<(String, String)>>` with `new(f: R) -> Self` and `get(&mut self, pid: u32) -> (String, String)` (returns `("unknown".into(), String::new())` on lookup failure, re-queries after a previous failure).

- [ ] **Step 1: Write the failing tests**

```rust
// tests/types.rs — serde round-trip and cache behaviour
#[test] fn connection_event_roundtrips_json() {
    let ev = ConnectionEvent { ts_ms: 1, pid: 2, process_name: "a.exe".into(), process_path: "C:\\a.exe".into(),
        remote_ip: "1.2.3.4".into(), remote_port: 443, local_port: 50000,
        protocol: Protocol::Tcp, direction: Direction::Out };
    let s = serde_json::to_string(&ev).unwrap();
    assert_eq!(serde_json::from_str::<ConnectionEvent>(&s).unwrap(), ev);
}
#[test] fn process_cache_queries_once_then_caches() {
    let mut calls = 0;
    let mut c = ProcessCache::new(|_pid| { calls += 1; Some(("x.exe".into(), "C:\\x.exe".into())) });
    let _ = c.get(7); let _ = c.get(7);
    assert_eq!(calls, 1);
}
#[test] fn process_cache_falls_back_and_retries() {
    let mut fail = true;
    let mut c = ProcessCache::new(move |_| if fail { None } else { Some(("y.exe".into(), String::new())) });
    assert_eq!(c.get(9).0, "unknown");
    fail = false;
    assert_eq!(c.get(9).0, "y.exe"); // retried after miss
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pcw-types` — Expected: FAIL (types not defined).

- [ ] **Step 3: Implement the types and cache in `crates/pcw-types/src/lib.rs`**

`ProcessCache` stores `HashMap<u32, Option<(String, String)>>` plus the closure; `get` consults the map, inserts on first query, replaces `None` entries on retry.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pcw-types` — Expected: 3 passed.

- [ ] **Step 5: Commit** — `git add crates/pcw-types && git commit -m "feat: shared event types and pid process cache"`.

---

### Task 3: Config and data directory

**Files:**
- Create: `crates/pcw-config/src/lib.rs`
- Test: `crates/pcw-config/tests/config.rs`

**Interfaces:**
- Produces:
  - `pub struct Config { pub retention_days: u32, pub resolve_dns: bool, pub capture_window_titles: bool, pub export_folder: PathBuf, pub export_schedule: String, pub dashboard_port: u16, pub udp_rate_cap: u32 }`
  - `pub fn default_data_dir() -> PathBuf` — `PCW_DATA_DIR` env var if set, else `%LOCALAPPDATA%\PC-Watcher` (`dirs::data_local_dir()`-equivalent via `std::env`), normalized with `dunce`-free plain `PathBuf`.
  - `impl Config { pub fn load_or_create(data_dir: &Path) -> Result<Config, ConfigError> }` — writes the spec §8 defaults to `data_dir/config.toml` when absent, then parses. Unknown keys tolerated; unreadable/invalid TOML → `ConfigError::Parse`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn creates_default_config_with_spec_values() {
    let dir = tempdir(); let cfg = Config::load_or_create(dir.path()).unwrap();
    assert_eq!(cfg.retention_days, 10); assert!(cfg.resolve_dns);
    assert!(cfg.capture_window_titles);
    assert_eq!(cfg.export_folder, PathBuf::from("G:\\My Drive\\PC-Watcher"));
    assert_eq!(cfg.export_schedule, "weekly"); assert_eq!(cfg.dashboard_port, 7411);
    assert_eq!(cfg.udp_rate_cap, 0);
    assert!(dir.path().join("config.toml").exists());
}
#[test] fn reloads_edited_values() { /* write config with retention_days = 5, reload, assert 5 */ }
#[test] fn invalid_toml_is_parse_error() { /* write "not toml", assert ConfigError::Parse */ }
#[test] fn data_dir_env_override_wins() {
    // set PCW_DATA_DIR to temp path, assert default_data_dir() == it
}
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-config` — Expected: FAIL.

- [ ] **Step 3: Implement `Config`, `default_data_dir`, `load_or_create`** with `serde::Deserialize` on `Config` using `#[serde(default = ...)]` per field so partial files still load.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-config` — Expected: 4 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: config with spec defaults and data-dir override"`.

---

### Task 4: Storage write path

**Files:**
- Create: `crates/pcw-storage/src/lib.rs`, `crates/pcw-storage/src/schema.sql` (or `const SCHEMA: &str`)
- Test: `crates/pcw-storage/tests/write.rs`

**Interfaces:**
- Consumes: `pcw-types::{ConnectionEvent, AppSession, Protocol, Direction}`.
- Produces:
  - `pub struct Store` (wraps `rusqlite::Connection`), `pub type SharedStore = Arc<Mutex<Store>>`.
  - `impl Store { pub fn open(path: &Path) -> Result<Store, StorageError> }` — creates parent dirs, enables `PRAGMA journal_mode=WAL`, `busy_timeout = 5000`, runs schema migration (idempotent), writes `meta.schema_version = 1`.
  - `pub fn open_readonly(path: &Path) -> Result<Store, StorageError>` — opens `file:...?mode=ro`; write attempts through it must return `StorageError`.
  - `insert_connection(&mut self, ev: &ConnectionEvent) -> Result<(), StorageError>` — **TCP lifecycle upsert**: key `(pid, remote_ip, remote_port, local_port, direction)` held in an in-memory `HashMap` of open connections → row id; new key inserts a row (`ts_first_ms = ts_last_ms = ts_ms`), existing key updates `ts_last_ms` only; a `Direction`/tuple mismatch inserts. **UDP inserts a new row every call.**
  - `insert_session(&mut self, s: &AppSession) -> Result<(), StorageError>`
  - `set_hostname(&mut self, ip: &str, host: &str) -> Result<(), StorageError>` (upsert), `set_meta(&mut self, key, val)`, `get_meta(&self, key) -> Option<String>`.
  - Schema exactly as spec §4.6: `connections(id, ts_first, ts_last, pid, process_name, process_path, remote_ip, remote_port, local_port, protocol, direction)`, `app_sessions(id, ts_start, ts_end, process_name, window_title)`, `hostnames(ip, hostname, resolved_at)`, `meta(key, value)`; indexes on `connections(process_name, ts_first)`, `connections(remote_ip)`, `connections(ts_first)`, `app_sessions(process_name, ts_start)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn open_creates_schema_and_version() { /* assert meta.schema_version == "1", 4 tables exist */ }
#[test] fn tcp_lifecycle_upserts_one_row() {
    // same key, ts 1000 then ts 5000 → 1 row, ts_first 1000, ts_last 5000
}
#[test] fn distinct_tcp_keys_insert_distinct_rows() { // different remote_port → 2 rows }
#[test] fn udp_datagrams_never_upsert() { // same key twice → 2 rows }
#[test] fn sessions_and_hostnames_roundtrip() { /* insert + reopen file + assert persisted */ }
#[test] fn readonly_store_rejects_writes() { /* open_readonly then insert_connection → Err */ }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-storage` — Expected: FAIL.

- [ ] **Step 3: Implement schema + write methods** per the Interfaces block; open-connection map lives on `Store`, rebuilt empty on `open()` (pre-existing rows keep their last `ts_last`).

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-storage` — Expected: 6 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: sqlite storage write path with tcp lifecycle upsert"`.

---

### Task 5: Storage read path and WAL concurrency

**Files:**
- Modify: `crates/pcw-storage/src/lib.rs` (new `queries` module)
- Test: `crates/pcw-storage/tests/read.rs`

**Interfaces:**
- Produces (consumed by Tasks 7, 8, 13):
  - `pub struct ConnectionRow { pub id: i64, pub ts_first_ms: u64, pub ts_last_ms: u64, pub process_name: String, pub process_path: String, pub remote_ip: String, pub remote_port: u16, pub local_port: u16, pub protocol: Protocol, pub direction: Direction, pub hostname: Option<String> }`
  - `pub struct OverviewSummary { pub total_connections: u64, pub top_apps: Vec<(String, u64)>, pub top_services: Vec<(String, u64)>, pub hourly: Vec<(u8, u64)> }` (hourly: `(local_hour 0..23, count)`)
  - `pub struct DestinationRow { pub remote_ip: String, pub remote_port: u16, pub protocol: Protocol, pub hostname: Option<String>, pub count: u64, pub ts_first_ms: u64, pub ts_last_ms: u64 }`
  - `pub struct UsageSummary { pub session_count: u64, pub total_secs: u64 }`
  - Methods on `Store`: `overview(&self, from_ms: u64, to_ms: u64) -> Result<OverviewSummary, StorageError>`; `app_destinations(&self, app: &str, from_ms: u64) -> Result<Vec<DestinationRow>, StorageError>`; `app_connections(&self, app: &str, from_ms: u64, limit: u32) -> Result<Vec<ConnectionRow>, StorageError>`; `service_connections(&self, key: &str, from_ms: u64) -> Result<Vec<ConnectionRow>, StorageError>` (matches `hostname = key` **or** `remote_ip = key`); `usage(&self, from_ms: u64) -> Result<Vec<AppSession>, StorageError>`; `usage_summary(&self, app: &str, from_ms: u64) -> Result<UsageSummary, StorageError>`; `connections_since(&self, ts_ms: u64) -> Result<Vec<ConnectionRow>, StorageError>`; `sessions_since(&self, ts_ms: u64) -> Result<Vec<AppSession>, StorageError>`; `unresolved_ips(&self, limit: u32) -> Result<Vec<String>, StorageError>` (ips in `connections` with no `hostnames` row, deduped).

- [ ] **Step 1: Write the failing tests**

Seed helper inserting known rows (fixed timestamps, apps `a.exe`/`b.exe`, ips `1.1.1.1`/`2.2.2.2`, hours 9 and 10).

```rust
#[test] fn overview_counts_and_orders_top_apps() { /* total, top_apps[0] == seeded winner, hourly has hours 9,10 */ }
#[test] fn app_destinations_groups_by_tuple() { /* one DestinationRow per ip:port with count */ }
#[test] fn service_connections_matches_hostname_or_ip() { /* set_hostname then query by hostname AND by ip */ }
#[test] fn usage_summary_sums_durations() { /* two sessions → session_count 2, total_secs correct */ }
#[test] fn connections_since_and_unresolved_dedupe() { /* only post-cutoff rows; ip with hostname excluded; dedupe */ }
#[test] fn reader_does_not_block_while_writer_writes() {
    // Task's Review Focus #1: two Stores on one file; write in a thread, query in a loop
    // Assert ≥1 successful read during 100 writes and zero Err from reads.
}
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-storage` — Expected: FAIL (new tests), old still pass.

- [ ] **Step 3: Implement the query methods** — all queries filter by `ts_first >= from_ms` (except `usage` on `ts_start`), all use the existing indexes; no `SELECT *` full scans without `WHERE ts_first >= ?`.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-storage` — Expected: 12 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: storage queries and wal concurrent read test"`.

---

### Task 6: Retention pruner with export guard

**Files:**
- Modify: `crates/pcw-storage/src/lib.rs` (`prune` method)
- Test: `crates/pcw-storage/tests/prune.rs`

**Interfaces:**
- Consumes: `Store::get_meta`, table schema from Task 4.
- Produces: `Store::prune(&mut self, retention_days: u32, now_ms: u64) -> Result<u64, StorageError>` — deletes `connections.ts_first` and `app_sessions.ts_start` older than `cutoff = min(now_ms - retention_days*86_400_000, last_export_ms)` where `last_export_ms` comes from `meta['last_export']`; **absent `last_export` ⇒ cutoff 0 ⇒ nothing deleted**. Returns rows deleted. `hostnames` cache is never pruned.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn deletes_rows_past_retention_once_exported() { // last_export = now; rows older than 10d → gone }
#[test] fn never_deletes_unexported_rows() {
    // Review Focus #2: rows 11 days old, last_export = 5 days ago → rows REMAIN (cutoff = last_export)
}
#[test] fn without_last_export_prunes_nothing() { // rows years old, no meta → all remain }
#[test] fn never_prunes_hostname_cache() { /* old hostnames survive a full prune */ }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-storage` — Expected: FAIL.

- [ ] **Step 3: Implement `prune`** — single transaction deleting both tables; `last_export` parsed as `u64` ms (corrupt value ⇒ treat as absent).

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-storage` — Expected: 16 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: export-guarded retention pruner"`.

---

### Task 7: Weekly export (CSV/JSON)

**Files:**
- Create: `crates/pcw-export/src/lib.rs`
- Test: `crates/pcw-export/tests/export.rs`, fixture `crates/pcw-export/tests/golden/`

**Interfaces:**
- Consumes: `SharedStore`, `Store::{connections_since, sessions_since, overview, set_meta, get_meta}`.
- Produces: `pub struct ExportReport { pub folder: PathBuf, pub connection_rows: u64, pub session_rows: u64 }`; `pub fn run_export(db: &SharedStore, export_folder: &Path, now_ms: u64) -> Result<ExportReport, ExportError>`.
- Output layout (spec §7), into `export_folder/YYYY-MM-DD/` (local date): `connections.csv` columns `ts_first, ts_last, process_name, process_path, remote_ip, remote_port, local_port, protocol, direction, hostname`; `app-usage.csv` columns `ts_start, ts_end, duration_secs, process_name, window_title`; `summary.json` `{total_connections, top_apps, top_services, range:{from_ms,to_ms}}`; `manifest.json` `{connection_rows, session_rows, from_ms, to_ms, generated_at_ms, schema_version, tool_version: env!("CARGO_PKG_VERSION"), dropped_events, watchdog_restarts}` (last two read from `meta`). Timestamps RFC 3339 local. CSV written with `csv::Writer` (RFC 4180 quoting). After a successful write: `set_meta("last_export", now_ms)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn hostile_window_title_survives_csv_roundtrip() {
    // Review Focus #5: title = "He said \"hi\",\ta\nnext line"
    // export → read back with csv::Reader → assert title byte-identical
}
#[test] fn manifest_carries_counts_and_version() { /* assert schema_version "1", tool_version == env!, dropped_events present */ }
#[test] fn second_export_is_incremental() { // export, add rows, export again → second folder only has new rows; first untouched
}
#[test] fn last_export_meta_set_after_success() { /* get_meta("last_export") == now_ms */ }
#[test] fn summary_json_shape_matches_spec() { /* keys exactly as Interfaces block */ }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-export` — Expected: FAIL.

- [ ] **Step 3: Implement `run_export`** — add `time` to `[workspace.dependencies]` (once, root); build the `YYYY-MM-DD` folder name from `now_ms` in **local** time via `time::OffsetDateTime::from_unix_timestamp` + `LocalOffset`; write the four files to a temp directory inside the export folder, then rename into place (crash-safe), then set `last_export`.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-export` — Expected: 5 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: incremental csv/json export with manifest"`.

---

### Task 8: DNS PTR resolver worker

**Files:**
- Create: `crates/pcw-resolve/src/lib.rs`
- Test: `crates/pcw-resolve/tests/resolve.rs`

**Interfaces:**
- Consumes: `SharedStore`, `Store::{unresolved_ips, set_hostname}`.
- Produces:
  - `pub trait NameResolver { fn lookup(&self, ip: &str) -> Option<String>; }`
  - `pub struct SystemResolver;` (impl via `dns_lookup::lookup_addr`, `Err` ⇒ `None`)
  - `pub fn resolve_once(db: &SharedStore, resolver: &impl NameResolver, limit: u32) -> Result<usize, ResolveError>` — look up up to `limit` unresolved ips, `set_hostname` each `Some`, return cached-count.
  - `pub fn run_worker(db: SharedStore, stop: Arc<AtomicBool>, interval: Duration, limit: u32)` — loops `resolve_once` with `SystemResolver` until `stop`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn resolve_once_caches_and_does_not_requery() {
    // Fake resolver counts calls; two runs → hostnames persisted, calls == number of distinct ips (not doubled)
}
#[test] fn failed_lookup_is_not_cached_and_retried_next_round() { /* fake returns None → still unresolved after run */ }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-resolve` — Expected: FAIL.

- [ ] **Step 3: Implement trait, `SystemResolver`, `resolve_once`, `run_worker`.**

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-resolve` — Expected: 2 passed.

- [ ] **Step 5: Manual smoke note** — add to `README.md` "Smoke checks": first PTR burst may trigger a Windows Firewall prompt for the agent; allow on private networks (spec §10 note). `git commit -m "feat: background ptr reverse-dns worker"`.

---

### Task 9: Foreground session state machine

**Files:**
- Create: `crates/pcw-foreground/src/lib.rs` (pure logic; no `windows` imports in this file)
- Test: `crates/pcw-foreground/tests/sessions.rs`

**Interfaces:**
- Consumes: `pcw-types::AppSession`.
- Produces:
  - `pub enum SessionSignal { ForegroundChange { ts_ms: u64, process_name: String, window_title: String }, Lock { ts_ms: u64 }, Sleep { ts_ms: u64 }, MonitorOff { ts_ms: u64 }, Resume { ts_ms: u64, foreground: Option<(String, String)> } }` (Resume's payload = `(process_name, window_title)` of current foreground if known).
  - `pub struct SessionTracker { /* open session + current app */ }` with `new() -> Self`, `on_signal(&mut self, s: SessionSignal) -> Option<AppSession>`, `finalize(&mut self) -> Option<AppSession>`.
  - Rules (spec §4.3): `ForegroundChange` to a *different* process closes the open session (at `ts_ms`) and opens a new one; change to the *same* process title-only updates the open session's `window_title` (no new session). `Lock`/`Sleep`/`MonitorOff` close the open session at `ts_ms` without opening one. `Resume` with `Some(fg)` opens a new session at `ts_ms`; with `None` opens nothing until the next change. `finalize` closes any open session at its stored end.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn app_switch_ends_and_starts() { /* a@0→switch b@100 ⇒ session a [0,100] opened, b open */ }
#[test] fn lock_ends_session_idle_does_not() {
    // Review Focus #4: Lock@100 closes [0,100]; no signal after → on_signal(None) impossible,
    // so assert: Lock closes; then Resume{Some} reopens; and a Tracker with no signals yields nothing on finalize until signaled.
}
#[test] fn sleep_and_monitor_off_close() { /* both variants close the open session */ }
#[test] fn resume_without_foreground_opens_nothing() { /* then ForegroundChange starts next session */ }
#[test] fn same_process_change_does_not_start_new_session() { /* title updates, session_count via finalize == 1 */ }
#[test] fn finalize_closes_open_session() { /* emits [start, finalize_ts] */ }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-foreground` — Expected: FAIL.

- [ ] **Step 3: Implement `SessionTracker`** as a fold over signals; durations computed by the caller from `ts_start/ts_end`.

- [ ] **Step 4: Run tests to verify they pass** — Run: `cargo test -p pcw-foreground` — Expected: 6 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat: foreground session state machine"`.

---

### Task 10: Foreground OS adapter (WinEvent + lock/power hooks)

**Files:**
- Create: `crates/pcw-foreground/src/os.rs`
- Test: `crates/pcw-foreground/tests/os_smoke.rs` (ignored-by-default manual test) + checklist in README

**Interfaces:**
- Consumes: `SessionTracker`, `SessionSignal`, `pcw_types::process::ProcessCache` (shared instance passed in as `Arc<Mutex<ProcessCache<_>>>`).
- Produces: `pub fn start_foreground_hook(tracker_tx: Sender<SessionSignal>, cache: Arc<Mutex<ProcessCache<...>>>, capture_titles: bool) -> Result<ForegroundHandle, OsError>` — spawns a thread owning a message loop; registers `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` plus session-lock (`WM_WTSSESSION_CHANGE`) and power/monitor-off (`PowerSettingRegisterNotification`) notifications; converts each to a `SessionSignal` on the channel. `ForegroundHandle::stop()` unhooks and ends the thread. When `capture_titles` is false, sends `window_title = ""`.

- [ ] **Step 1: Implement the hook thread** using the `windows` crate (only this file and `pcw-etw` may import it): `SetWinEventHook` with `WINEVENT_OUTOFCONTEXT` on its own thread, `GetForegroundWindow` + `GetWindowTextW` + `GetWindowThreadProcessId` inside the callback, `WTSRegisterSessionNotification` for lock/unlock, `PowerSettingRegisterNotification` for `GUID_MONITOR_POWER_OFF` and suspend.

- [ ] **Step 2: Write the ignored manual test harness**

`#[test] #[ignore = "manual: needs interactive session"]` — starts the hook, prints signals for 60 s so a human can alt-tab, lock, unlock.

Run: `cargo test -p pcw-foreground -- --ignored --nocapture` — Expected: ForegroundChange on alt-tab; Lock on Win+L; no ForegroundChange while idle.

- [ ] **Step 3: Add the smoke checklist to README** — "alt-tab produces sessions with titles; Win+L closes the open session; monitor-off closes it."

- [ ] **Step 4: Commit** — `git commit -m "feat: win event and power hooks feeding session tracker"`.

---

### Task 11: ETW network source with watchdog

**Files:**
- Create: `crates/pcw-etw/src/lib.rs`, `crates/pcw-etw/src/source.rs`, `crates/pcw-etw/src/supervisor.rs`, `crates/pcw-etw/src/rate.rs`, `crates/pcw-etw/examples/capture.rs`
- Test: `crates/pcw-etw/tests/supervisor.rs`, `crates/pcw-etw/tests/rate.rs`

**Interfaces:**
- Consumes: `pcw-types::{ConnectionEvent, Protocol, Direction, ProcessCache}`.
- Produces:
  - `pub trait NetSource { fn run(&mut self, sink: &mut dyn FnMut(ConnectionEvent)) -> Result<(), SourceError>; }` (blocking; `Err` = session died).
  - `pub struct EtwNetSource { /* ETW session on Microsoft-Windows-TCPIP */ }` — decodes connect/accept → `Direction::Out`/`In` respectively, TCP disconnect closes the lifecycle key upstream (via `insert_connection` receiving only open/close/transfer events; closes simply refresh `ts_last`), UDP datagrams insert per event. Dead pid ⇒ process_name `"unknown"`.
  - `pub fn query_process(pid: u32) -> Option<(String, String)>` — the **only** Win32 process lookup (`OpenProcess` + `QueryFullProcessImageNameW`), exposed so `pcw-agent` can build the one shared `ProcessCache` without importing `windows`.
  - `pub struct Supervisor { pub restarts: u64, pub dropped: u64, backoff: Duration }` with `on_event(&mut self, ev: SupervisedEvent) -> SupervisorAction` where `SupervisedEvent = SourceExited(Result<(), SourceError>) | SessionLost { dropped: u64 }` and `SupervisorAction = Restart { wait: Duration } | KeepGoing`, backoff 1 s doubling to a 60 s cap, restarts unbounded.
  - `pub struct RateLimiter` with `allow(&mut self, key: &str, now_ms: u64) -> bool` — enforces `udp_rate_cap` per `(pid,ip,port)` tuple; `cap == 0` ⇒ always true. Dropped-by-cap counted into `Supervisor.dropped`.

- [ ] **Step 1: Write the failing supervisor tests**

```rust
#[test] fn restarts_with_doubling_backoff_capped_at_60s() {
    // three SourceExited(Err) in a row → waits 1s, 2s, 4s … cap 60s; restarts counts each
}
#[test] fn session_loss_adds_dropped_and_keeps_running() { /* SessionLost{5} → dropped 5, KeepGoing */ }
#[test] fn clean_exit_still_restarts() { /* Ok(()) exit → Restart (collector must not silently stop) */ }
```

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-etw` — Expected: FAIL.

- [ ] **Step 3: Write the failing rate-limiter tests** — `cap=0` allows 1000 same-key in a second; `cap=5` allows 5 then rejects until `now_ms` advances 1000 ms.

- [ ] **Step 4: Run tests to verify they fail** — Run: `cargo test -p pcw-etw` — Expected: rate tests FAIL too.

- [ ] **Step 5: Implement `Supervisor`, `RateLimiter`, `query_process`.**

- [ ] **Step 6: Run tests to verify they pass** — Run: `cargo test -p pcw-etw` — Expected: 5 passed.

- [ ] **Step 7: Implement `EtwNetSource` + `examples/capture.rs`** using the `etw` crate: start a private session, subscribe `Microsoft-Windows-TCPIP`, decode to `ConnectionEvent`, honour `udp_rate_cap` from config. Example prints events for 10 s.

- [ ] **Step 8: Manual ETW smoke** — Run: `cargo run -p pcw-etw --example capture` while browsing. Expected: rows appear with correct process names (`msedge.exe` etc.), ports 443, `Out` direction; hibernate the machine and confirm the supervisor logs a restart + `dropped` bump (this is the spec §4.1 watchdog acceptance).

- [ ] **Step 9: Commit** — `git commit -m "feat: etw net source with supervisor watchdog and udp rate cap"`.

---

### Task 12: Agent binary (wiring, writer, tray, scheduler)

**Files:**
- Create: `crates/pcw-agent/src/main.rs`, `crates/pcw-agent/src/writer.rs`, `crates/pcw-agent/src/tray.rs`, `crates/pcw-agent/src/install.rs`, `crates/pcw-agent/src/timers.rs`

**Interfaces:**
- Consumes: everything prior — `Config::load_or_create(default_data_dir())`, `Store::open(data_dir/data.db)` → `SharedStore`, `EtwNetSource` + `Supervisor` + `query_process` (Task 11), `start_foreground_hook` + `SessionTracker` (Tasks 9–10), `resolve_once`/`run_worker` (Task 8), `run_export` (Task 7), `Store::prune` (Task 6).
- Produces:
  - `writer::run_writer(rx: Receiver<WriterMsg>, db: SharedStore)` — flushes in one transaction every 1 s **or** 500 queued events, whichever first. `enum WriterMsg { Connection(ConnectionEvent), Session(AppSession), Shutdown }`.
  - `main()`: argv `--install` → `install::install()`; `--uninstall` → `install::uninstall()`; otherwise run the agent.
  - `install::install()` runs `schtasks /Create /TN PC-Watcher /TR "\"<current_exe>\"" /SC ONLOGON /RL HIGHEST /F` via `std::process::Command`; `uninstall()` runs `/Delete /TN PC-Watcher /F`. Non-zero exit → `Err` with stderr text.
  - Tray menu items exactly (Global Constraints): **Open Dashboard** (spawn sibling `pcw-dash.exe`), **Pause / Resume** (toggles `Arc<AtomicBool>` the ETW sink checks; tracker keeps running), **Export now** (spawns `run_export` on a thread), **Open data folder** (`explorer.exe <data_dir>`), **Exit** (graceful: stop hooks, drain writer, flush `Shutdown`).
  - Timers: prune once an hour (checks daily), export when `now - last_export >= 7 days` (`export_schedule = "weekly"`), both on one background thread with `std::thread::sleep`.

- [ ] **Step 1: Implement `writer.rs`** with the batch-flush contract (exact 1 s / 500-event rule from Interfaces).

- [ ] **Step 2: Write the failing writer test** (in `crates/pcw-agent/tests/writer.rs`): send 3 `Connection` msgs, assert they appear in a temp DB within 1.5 s; send `Shutdown`, assert channel drained and DB closed cleanly.

Run: `cargo test -p pcw-agent` — Expected: FAIL.

- [ ] **Step 3: Implement writer to pass** — Run: `cargo test -p pcw-agent` — Expected: 1 passed.

- [ ] **Step 4: Implement `main.rs`, `install.rs`, `timers.rs`** wiring per Interfaces; the ETW sink closure: `if !paused.load() { tx.send(Connection(ev)) }` — everything else (cache, rate-limit, supervisor) already inside Task 11 types.

- [ ] **Step 5: Implement `tray.rs`** — `tray-icon` + `tao` event loop on the main thread (per tray-icon's required message pump); other workers on spawned threads.

- [ ] **Step 6: Manual smoke — install and footprint** (spec §6, §12)

Run: `cargo build --release && target\release\pcw-agent.exe --install`
Checklist: (a) reboot/logon → agent starts elevated with **no UAC prompt**; (b) tray icon present with all five items; (c) Task Manager after 10 min idle: memory **< 40 MB**, CPU average **< 1%**; (d) Pause stops `connections` growth in DB, Resume restarts; (e) Export now creates the dated folder; (f) `--uninstall` removes the scheduled task.

- [ ] **Step 7: Commit** — `git commit -m "feat: agent binary with tray, writer, timers, schtasks install"`.

---

### Task 13: Dashboard server and embedded UI

**Files:**
- Create: `crates/pcw-dashboard/src/lib.rs`, `crates/pcw-dashboard/src/routes.rs`, `crates/pcw-dashboard/src/ui.html`
- Test: `crates/pcw-dashboard/tests/server.rs`

**Interfaces:**
- Consumes: `Store::{overview, app_destinations, app_connections, service_connections, usage, usage_summary}` (Task 5), `Config.dashboard_port`.
- Produces:
  - `pub fn serve(store: Store, port: u16, stop: Arc<AtomicBool>) -> Result<(), DashError>` — binds **`127.0.0.1` only** (never `0.0.0.0`/`[::]`), serves until `stop`.
  - Routes: `GET /` → embedded `ui.html` (`include_str!`); `GET /api/overview?from&to` → `OverviewSummary` JSON; `GET /api/app?name&from&limit` → `{"destinations": [...], "connections": [...], "usage": {...}}`; `GET /api/service?key&from` → connections; `GET /api/usage?from` → sessions JSON. Unknown route → 404. Port in use → `DashError::Bind`.
  - `ui.html`: single file, vanilla JS, hand-rolled SVG bars, four views (Overview, App drill-down, Service drill-down, Usage), no external references.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn binds_loopback_only() { /* serve on port 0-ish free port; assert local_addr().ip() == 127.0.0.1 and NOT reachable via LAN ip */
                                  // practical form: bind succeeds, and the SocketAddr reported is 127.0.0.1 }
#[test] fn overview_endpoint_returns_seeded_json() { /* raw TcpStream GET → 200, body contains seeded app name */ }
#[test] fn app_endpoint_returns_drill_down_shape() { /* assert keys destinations/connections/usage */ }
#[test] fn unknown_route_404s() { /* assert "404" status line */ }
#[test] fn ui_has_no_external_urls() { /* ui.html contains no "http://" nor "https://" nor "//cdn" */ }
```
(HTTP over raw `std::net::TcpStream` in tests; no new deps.)

- [ ] **Step 2: Run tests to verify they fail** — Run: `cargo test -p pcw-dashboard` — Expected: FAIL.

- [ ] **Step 3: Implement `serve` + `routes.rs`.**

- [ ] **Step 4: Write `ui.html`** — four views wired to the five endpoints, drill-down navigation (click app → its destinations and event rows; click service → apps that talked to it), SVG hourly chart, filters `from`/`to`.

- [ ] **Step 5: Run tests to verify they pass** — Run: `cargo test -p pcw-dashboard` — Expected: 5 passed.

- [ ] **Step 6: Commit** — `git commit -m "feat: loopback dashboard server with embedded offline ui"`.

---

### Task 14: `pcw-dash` binary and end-to-end verification

**Files:**
- Create: `crates/pcw-dash/src/main.rs`
- Modify: `README.md` (final: install, config reference, smoke script)

**Interfaces:**
- Consumes: `Config::load_or_create`, `Store::open_readonly` (never opens for write — agent stays sole writer), `pcw_dashboard::serve`.
- Produces: `pcw-dash.exe [--port N]` — defaults `config.dashboard_port` (7411); opens read-only store; binds; launches default browser to `http://127.0.0.1:<port>/` (`cmd /c start <url>`); exits on Ctrl+C or after 3600 s with zero requests (idle timeout).

- [ ] **Step 1: Implement `pcw-dash` main** per Interfaces: `--port` override, readonly open (a readonly-open failure prints "is pcw-agent running? data not yet created" and exits 1), browser launch after successful bind.

- [ ] **Step 2: End-to-end verification pass** (the spec's real acceptance)

Manual script in README, executed once and results noted in the PR/commit message:
1. `pcw-agent --install`, logon → no UAC, tray present, footprint within §12.
2. Browse for 10 minutes → run `pcw-dash` → Overview shows the activity; click an app → destinations + individual connection rows with times, ports, resolved hostnames; click a service → the apps that talked to it; Usage shows sessions with window titles.
3. Lock the machine for 5 min mid-session → Usage duration excludes it (Review Focus #4).
4. `Export now` → open `connections.csv`/`app-usage.csv`/`summary.json`/`manifest.json` in the configured Drive folder; titles intact.
5. Kill the agent process, restart it → watchdog resumed collection; `manifest.json` next export shows nonzero `watchdog_restarts` if a loss occurred.
6. Verify from another device on the LAN that `http://<lan-ip>:7411` is unreachable while the dashboard is running (Review Focus #3).

- [ ] **Step 3: Final README** — install/uninstall, config reference (exact §8 keys), smoke checklist, `cargo --timings` note, firewall note from Task 8.

- [ ] **Step 4: Commit** — `git commit -m "feat: on-demand dashboard binary and e2e verification"`.

---

## Self-Review Notes (for the executor)

- Spec §4.5 (PTR worker) → Task 8; §4.7 (pruner) → Task 6; §7 (export layout) → Task 7; §9 resolved decisions → Tasks 4 (raw), 6 (export guard), 9 (session rules), 10 (titles), 12 (install); §11 build strategy → Tasks 1–2 and every task's workspace-deps usage; §13 testing notes → each task's test steps.
- Type consistency: `ConnectionEvent`/`AppSession` defined once (Task 2); `SharedStore` once (Task 4); query DTOs once (Task 5); signals once (Task 9).
- Windows-only tasks (10, 11, 12) carry manual smoke checklists because automated tests cannot lock a screen or hibernate a machine — this is deliberate, not a gap.
