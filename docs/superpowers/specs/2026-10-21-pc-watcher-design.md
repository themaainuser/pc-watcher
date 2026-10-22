# PC-Watcher — Design Document

Date: 2026-10-21
Status: Awaiting owner approval

## 1. Intent and Why

A personal, local-only surveillance tool for one Windows PC. The owner wants to
answer, after the fact:

- Which application talked to which internet service, and when?
- Which app was in the foreground, how many times, and for how long — and what
  was on screen (window title)?
- When something odd appears in the network log, drill into it: which app, which
  destination, what time, what port.

Explicitly **not** a security product, not a packet sniffer, and not a remote
monitoring tool. It records **metadata only**: process name, window title,
destination IP, port, protocol, timestamps. It never records payloads, URLs,
page contents, keystrokes, or file contents.

## 2. Goals

- Always-on, silent background collection with a small footprint
  (target: < 40 MB RAM, < 1% average CPU while idle — revised for raw rows).
- Accurate, best-effort per-process attribution of every outbound and inbound
  connection, stored **one row per connection** (owner decision: raw, not
  merged).
- Foreground app-usage tracking: which app, session count, total duration,
  window title per session. Lock/sleep ends a session; idle does not.
- An on-demand dashboard the user starts only when curious; zero footprint
  otherwise.
- Drill-down: app → its destinations → individual connection events.
- Service name resolution (IP → hostname like `api.github.com`) via local
  reverse DNS, on by default, disable-able.
- 10–14 days local retention with automatic pruning that never deletes
  unexported data.
- Weekly structured export (CSV/JSON) into a folder Google Drive for Desktop
  syncs.

## 3. Non-Goals / Anti-Goals

- No packet capture, no Npcap/WinPcap driver installs. (ETW only.)
- No payloads, URLs, DNS query contents, HTTP headers, or decryption. Ever.
- No keystroke, clipboard, screenshot, or file-content data. Window **titles**
  are in scope (owner decision) — nothing deeper.
- No network exposure: the dashboard binds 127.0.0.1 only. No cloud API calls
  from the app. No telemetry.
- No blocking, throttling, or firewalling — observe only.
- No anomaly alerts, no ML, no "insights" in v1.
- No cross-platform support. Windows 10/11 x64 only.

## 4. How It Works

### 4.1 Collection: ETW, not packets

The agent subscribes to the **`Microsoft-Windows-TCPIP` ETW provider** (kernel
networking events). This yields connection-level events — TCP connect, accept,
disconnect, UDP datagrams — carrying process ID, local/remote address, port, and
timestamp. No packet contents ever reach the app. ETW is a push stream from the
OS: the app registers a callback and receives events; it does not poll.

Requires elevation — see §6.

**Watchdog (builder call):** real-time ETW sessions can drop events on buffer
overrun and die if the agent crashes or the machine hibernates. The agent
detects session loss, restarts the session automatically, and records a
dropped-event counter in `meta`. "Every connection" is thus best-effort with
loss visible, not silently assumed complete.

**UDP thinning fallback (builder call):** the provider emits UDP events per
datagram (QUIC, calls, telemetry), which can dwarf TCP volume. Recording every
datagram is the v1 default. If measurement shows raw UDP volume breaching the
§12 CPU/RAM targets, a documented filter stage (per-tuple rate cap, skip
loopback/broadcast) becomes available via config. No accuracy promise is
quietly broken — the cap is explicit and off by default.

### 4.2 PID → process name

For each unseen PID: `OpenProcess` + `QueryFullProcessImageName`. Cached in
memory (PID + creation-time key) so each process is queried once. Process
executable path is stored; command lines are **not** collected.

### 4.3 Foreground app tracking

`SetWinEventHook(EVENT_SYSTEM_FOREGROUND, ...)` — an OS-driven callback on every
foreground window change. Zero polling, zero idle cost. From the HWND we get the
PID → process name (same cache) **and the window title** (owner decision:
captured; goes into `app_sessions` and the usage export).

**Session-end rule (owner decision):** a session ends when the foreground moves
to a different process **or** on screen lock / sleep / hibernate / monitor-off.
Idle without lock does **not** end a session. So "3 h in VS Code" means time the
app was actually visible and unlocked. Implemented by also hooking the
session-lock and power events via the same `windows`-crate surface.

### 4.4 Raw rows (owner decision)

Every connection event is stored as its own row with its own timestamp. No
write-time merging. `raw_mode` and `merge_window_secs` from earlier drafts are
**removed** — there is one mode, and it is raw. This is the owner's explicit
choice, accepting ~15–60 MB/day of DB growth for maximum forensic granularity:
each row is one connection, individually timestamped.

### 4.5 DNS service-name resolution

A single background worker thread pulls unresolved IPs from the DB, performs
**PTR reverse lookups** through the system's configured DNS resolver, caches
results in a `hostnames` table (ip → hostname, resolved_at), and never blocks
the collector. Note: a PTR query does travel to your configured DNS server
(usually your router/ISP) — it contains only the IP you already connected to.
Disable-able via `resolve_dns = false`.

### 4.6 Storage: SQLite (WAL)

Single file, WAL mode so the dashboard can read while the agent writes.
Location: **`%LOCALAPPDATA%\PC-Watcher\data.db`** — deliberately not on the
Desktop, because the user's Desktop is OneDrive-synced and a live SQLite file
inside a sync client causes lock conflicts.

Schema (v1):

```
connections(
  id INTEGER PK, ts_first INT, ts_last INT,
  pid INT, process_name TEXT, process_path TEXT,
  remote_ip TEXT, remote_port INT, local_port INT,
  protocol TEXT,              -- 'tcp' | 'udp'
  direction TEXT              -- 'out' | 'in'
)
app_sessions(
  id INTEGER PK, ts_start INT, ts_end INT,
  process_name TEXT, window_title TEXT
)
hostnames(ip TEXT PK, hostname TEXT, resolved_at INT)
meta(key TEXT PK, value TEXT)  -- schema version, last_export, dropped_event
                               -- counters, watchdog restarts
```

(`ts_last` remains for connection-lifetime display on long-lived TCP
connections; with raw rows each connection still occupies exactly one row from
open to close.)

Indexes: `connections(process_name, ts_first)`, `connections(remote_ip)`,
`connections(ts_first)`, `app_sessions(process_name, ts_start)`.

**Estimated footprint (raw):** ~15–60 MB/day depending on activity; 14-day
retention worst case ~850 MB, typical ~300–500 MB. The owner accepted this.

### 4.7 Retention — unexported data is never deleted

A daily maintenance pass deletes rows older than `retention_days`
(default 10). **Builder call:** the pruner's cutoff is
`min(retention_cutoff, last_successful_export_time)` — the pruner will not
delete anything that hasn't been exported at least once. A failed weekly export
(sick Drive client, machine off for two weeks) can grow the DB past the target
size, but it cannot lose data. No per-row flag needed; `meta.last_export`
drives it.

## 5. The Two Binaries

| Binary | Lifetime | Role |
|---|---|---|
| `pcw-agent.exe` | Always (tray icon, auto-start at logon) | ETW collector + watchdog, foreground tracker, DNS worker, writer, pruner, weekly exporter |
| `pcw-dash.exe` | On demand | Opens the SQLite file read-only, serves the dashboard at `http://127.0.0.1:7411`, launches the default browser, exits on demand or timeout |

Splitting them is what keeps the footprint promise honest: when the user is not
looking, no web server, no UI framework, nothing extra is resident.

### 5.1 Agent tray menu

- Open Dashboard (launches `pcw-dash.exe`)
- Pause / Resume collection
- Export now
- Open data folder
- Exit

### 5.2 Dashboard pages (single embedded HTML file, vanilla JS, offline)

No CDN, no npm build. One HTML file compiled into the binary
(`include_str!`), hand-rolled SVG charts so it works with no internet.

- **Overview** — today's totals, top apps by connections, top services, hourly
  activity chart.
- **App drill-down** — pick an app → destinations with counts/time ranges,
  foreground usage (sessions, total time, recent window titles), event list.
- **Service drill-down** — pick a hostname/IP → which apps talked to it and when.
- **Usage** — foreground sessions timeline with window titles, totals per app.
- Filters: date range, app, protocol, port, direction.

## 6. Privileges and Auto-Start

Enabling an ETW kernel networking session requires elevation. Foreground-window
hooks must run inside the user's desktop session (a Windows service runs in
session 0 and cannot see user windows — services are ruled out).

**Confirmed:** the owner's account **is** a local administrator under default
UAC. A Windows **scheduled task** at logon with `/rl highest` (run with highest
privileges) therefore starts the agent elevated in the user session with no UAC
prompt and no stored password. Install/uninstall via
`pcw-agent.exe --install` / `--uninstall`, wrapping `schtasks`. One-time
elevated run for `--install` is accepted.

## 7. Export to Google Drive

The user's Google Drive for Desktop client already syncs a local folder. The
agent writes structured files there; Drive handles upload. No API keys, no OAuth.

```
<export_folder>/               e.g. G:\My Drive\PC-Watcher\
└── 2026-10-21/                one folder per export date
    ├── connections.csv        every row since last export
    ├── app-usage.csv          foreground sessions (incl. window titles)
    ├── summary.json           totals, top apps, top services, range
    └── manifest.json          row counts, time range, generated_at,
                               schema version, tool version,
                               dropped-event counters
```

Schedule: weekly (configurable). Also manual via tray "Export now".
Exports are incremental (`meta.last_export`); the first export ships full
history. `manifest.json` carries the ETW dropped-event counters so archived
data carries its own accuracy caveat.

## 8. Config

`%LOCALAPPDATA%\PC-Watcher\config.toml`, created on first run with defaults:

```toml
retention_days = 10
resolve_dns = true
capture_window_titles = true    # owner decision: on
export_folder = "G:\\My Drive\\PC-Watcher"
export_schedule = "weekly"
dashboard_port = 7411
udp_rate_cap = 0                # 0 = off; thinning fallback, §4.1
```

## 9. Resolved Decisions

| Question | Decision |
|---|---|
| Admin account | Yes — `/rl highest` logon task elevates silently |
| Connection rows | **Raw**, one row per connection, no merging |
| Window titles | **Captured** in `app_sessions` and usage export |
| Session end | App-switch, lock, sleep, hibernate, monitor-off; idle does not end |
| Unexported data | Pruner never deletes past `last_export` |
| ETW reliability | Watchdog + restart + dropped-event counters |
| UDP flood | Record all; explicit config cap as documented fallback |

Still open (config values, not design): the actual Google Drive export folder
path on this machine; merge-window tuning is gone (raw mode).

## 10. Constraints

- Windows 10/11 x64. Rust stable toolchain.
- Local administrator account (confirmed).
- Google Drive for Desktop assumed installed and signed in (only for export
  syncing; the app itself never talks to Google).
- The only network traffic the app itself generates is optional DNS PTR
  lookups through the system resolver.
- OneDrive-synced Desktop exists on this machine — the DB must stay in
  `%LOCALAPPDATA%`, never inside a sync root.

## 11. Build-Performance Strategy (from the r/rust thread)

The referenced thread's consensus, applied from day one:

1. **Cargo workspace of small crates**, one `main.rs`, everything else
   `lib.rs`. Shallow dependency graph between crates (no deep chains) so
   compilation parallelizes.
2. **`[workspace.dependencies]`** at the root; members use
   `dep = { workspace = true }`. One pinned version of every dependency —
   no duplicate serde/anyhow versions bloating builds.
3. **Fast linker:** `rust-lld` (ships with the toolchain) enabled via
   `.cargo/config.toml` — linking, not compiling, is usually the wall.
4. **Isolate the heavy dependency:** the `windows` crate (notorious compile
   times) lives only in the two crates that need it (`pcw-etw`,
   `pcw-foreground`) with minimal explicit features. Editing dashboard code
   never recompiles it.
5. **Dev profile:** incremental on (default); optional Cranelift backend on
   nightly for very fast debug builds — documented, not required.
6. **Release profile:** `lto = "thin"`, `opt-level = 3`, `strip = true`.
7. `cargo --timings` documented in the README for bottleneck hunting.

### Crate layout

```
pc-watcher/
├── Cargo.toml              # workspace + [workspace.dependencies]
├── .cargo/config.toml      # lld linker
├── crates/
│   ├── pcw-types           # shared structs, zero heavy deps
│   ├── pcw-config          # TOML config load/save
│   ├── pcw-storage         # SQLite schema, writes, queries, pruning
│   ├── pcw-etw             # ETW TCPIP collector + watchdog (windows crate)
│   ├── pcw-foreground      # WinEvent + lock/power hooks   (windows crate)
│   ├── pcw-resolve         # DNS PTR worker
│   ├── pcw-export          # CSV/JSON weekly export
│   ├── pcw-dashboard       # localhost server + embedded UI
│   ├── pcw-agent           # bin: tray, wiring, scheduler
│   └── pcw-dash            # bin: on-demand dashboard
```

## 12. Performance Targets (revised for raw rows)

- Agent idle: < 40 MB RAM, < 1% CPU average.
- ETW buffer: small (64–128 KB), batched DB inserts (one transaction per
  flush, ~1 s cadence) — never one transaction per event.
- Dashboard cold start to browser paint: < 3 s on 14-day-old raw data
  (queries are indexed; no full scans on the hot pages).
- DB file at steady state (raw rows, 14-day retention): typical 300–500 MB,
  worst case ~850 MB. Export-guarded pruning may temporarily exceed this.

## 13. Testing Notes (for the planner)

- Storage: temp-file SQLite integration tests (insert, query, prune-vs-export
  guard).
- ETW/foreground/lock hooks: thin OS adapters behind traits so session and
  retention logic stays testable; a manual smoke checklist covers the OS edge.
- Export: golden-file tests on CSV/JSON output, including window titles and
  manifest counters.
- Watchdog: fault-injection test that kills the ETW session and asserts
  restart + counter increment (manual or semi-automated).
