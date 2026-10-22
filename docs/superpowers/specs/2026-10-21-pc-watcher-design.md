# PC-Watcher — Design Document

Date: 2026-10-21 (revised 2026-10-22)
Status: Revised after build-readiness review — awaiting owner approval

## 0. Revision 2026-10-22 — what changed and why

A review against this PC, the Windows event manifests on it, and the actual
library sources found problems that would have broken the build or recorded
wrong data. Every API, event ID, field name and crate version named below was
checked on this machine (a throwaway crate using them compiles and links).

| Change | Why | Section |
|---|---|---|
| Network events come from `Microsoft-Windows-Kernel-Network`, not `Microsoft-Windows-TCPIP` | TCPIP's connect event (1002) has no process ID; Kernel-Network has a `PID` field on every connect, accept, close and UDP event | §4.1 |
| ETW library is `ferrisetw` 1.2.0 | the `etw` crate is an empty 2019 placeholder (v0.0.1) | §4.1, §11 |
| Ports from ETW are byte-swapped before use | Kernel-Network ports are network byte order (`win:Port`); ferrisetw reads integers in native order, so 443 would read as 47873 | §4.1 |
| Connections already open at start are read from the OS connection table | no connect event ever arrives for them | §4.1 |
| Connection events say Open, Close or Packet | storage could not tell open from close; the open-connection map grew forever | §4.4 |
| Process cache entry dropped when ETW reports a new process with that PID | Windows reuses PIDs; a PID-only cache mislabels connections | §4.2 |
| UDP stays one row per datagram (owner decision); size estimate redone; low-disk guard added | the old 15–60 MB/day estimate ignored per-datagram rows | §4.4, §4.6 |
| Every window-title change is logged (owner decision) | one title per session lost what was on screen | §4.3, §4.6 |
| Names come from the Windows DNS client, reverse DNS as fallback (owner decision); failed lookups remembered for 24 h | reverse DNS rarely gives names like `api.github.com`; uncached failures starved new IPs | §3, §4.5 |
| Session tracker has locked / asleep / display-off / paused state; the lock screen app never counts | waking or turning the display on while still locked would start a session | §4.3 |
| Export uses row-ID watermarks and streams rows | a time watermark skipped rows still in the write batch, which the pruner later deleted; loading a week into memory broke the RAM target | §4.7, §7 |
| Scheduled task created from XML: no 72 h limit, runs on battery | `schtasks` defaults stop a task after 72 h and on battery; this PC is a laptop | §6 |
| Binaries have no console window, are installed to `%LOCALAPPDATA%`, and log to a file | otherwise a console opens at every logon, and the build folder is inside OneDrive | §5, §6 |
| Browser opened through `explorer.exe` | the elevated agent would otherwise start the browser as admin | §5 |
| Build output kept out of OneDrive | OneDrive syncs and locks the multi-GB `target/` folder | §10 |

## 1. Intent and Why

A personal, local-only surveillance tool for one Windows PC. The owner wants to
answer, after the fact:

- Which application talked to which internet service, and when?
- Which app was in the foreground, how many times, and for how long — and what
  was on screen (window titles)?
- When something odd appears in the network log, drill into it: which app, which
  destination, what time, what port.

Explicitly **not** a security product, not a packet sniffer, and not a remote
monitoring tool. It records **metadata only**: process name, window title,
destination IP, port, protocol, timestamps, and the service name for IPs it
saw. It never records payloads, URLs, page contents, keystrokes, or file
contents.

## 2. Goals

- Always-on, silent background collection with a small idle footprint
  (target: < 40 MB RAM, < 1% average CPU while idle). Under heavy UDP traffic
  CPU and disk use grow with the traffic; they are measured and recorded
  (§12), not promised.
- Best-effort per-process attribution of every outbound and inbound
  connection. Raw storage (owner decisions): **TCP = one row per connection**
  (open → close); **UDP = one row per datagram**.
- Foreground app-usage tracking: which app, session count, total duration, and
  every window title shown during the session with its time. Lock, sleep and
  display-off end a session; idle does not.
- An on-demand dashboard the user starts only when curious; zero footprint
  otherwise.
- Drill-down: app → its destinations → individual connection events.
- Service names (IP → name like `api.github.com`): the name the PC looked up,
  taken from the Windows DNS client; reverse DNS as fallback. Both on by
  default, each can be turned off.
- 10–14 days local retention (default 10) with automatic pruning that never
  deletes unexported data.
- Weekly structured export (CSV/JSON) into a folder Google Drive for Desktop
  syncs.

## 3. Non-Goals / Anti-Goals

- No packet capture, no Npcap/WinPcap driver installs. (ETW only.)
- No payloads, URLs, HTTP headers, or decryption. Ever.
- DNS (revised by owner 2026-10-22): the agent reads the answers to lookups the
  PC already makes (name → IPs) only to label recorded connections. A name is
  stored only for an IP that appears in a recorded connection; no log of DNS
  queries is kept. The agent makes no DNS queries of its own except optional
  reverse lookups (§4.5).
- No keystroke, clipboard, screenshot, or file-content data. Window **titles**
  are in scope, including every change (owner decision) — nothing deeper.
- No network exposure: the dashboard binds 127.0.0.1 only. No cloud API calls
  from the app. No telemetry.
- No blocking, throttling, or firewalling — observe only.
- No anomaly alerts, no ML, no "insights" in v1.
- No cross-platform support. Windows 10/11 x64 only.

## 4. How It Works

### 4.1 Collection: ETW, not packets

The agent runs one real-time ETW session named **`PC-Watcher-Net`** (a normal
named session — "private" logger sessions cannot deliver events in real time)
through `ferrisetw` 1.2.0, with three providers:

| Provider | GUID | Keywords | Event IDs used |
|---|---|---|---|
| Microsoft-Windows-Kernel-Network | `7dd42a49-5329-4832-8dfd-43d979153a88` | `0x30` (IPv4 + IPv6) | TCP connect 12/28 (out), accept 15/31 (in), disconnect 13/29; UDP send 42/58 (out), receive 43/59 (in) |
| Microsoft-Windows-DNS-Client | `1c95126e-7eea-49a9-a3fe-a378b03ddb4d` | all | 3008 (query completed) |
| Microsoft-Windows-Kernel-Process | `22fb2cd6-0e7b-422b-a0c7-2fad1fd0e716` | `0x10` (process) | 1 (start), 2 (stop) |

Event-ID filters (`ferrisetw` `EventFilter::ByEventIds`) keep every other event
out of the agent — including the very high-volume TCP data events
(10/11/26/27), which are never needed. ETW is a push stream: the agent
registers callbacks; it does not poll.

Kernel-Network event fields: `PID, size, daddr, saddr, dport, sport, …,
connid`. Ports are typed `win:Port` (network byte order) and must be converted
with `u16::from_be`; addresses parse correctly as `IpAddr`.

Why not Microsoft-Windows-TCPIP: in its manifest, the connect event 1002
("Tcb … requested to connect") has no PID; PIDs appear only on endpoint-creation
and failure events, so attribution would need correlating TCBs to endpoints.

**Connections open before collection starts** never produce a connect event.
Each time the session starts (first start and every watchdog restart), the
agent reads the OS TCP table (`GetExtendedTcpTable`, `TCP_TABLE_OWNER_PID_ALL`,
IPv4 and IPv6; ports in that table are also network byte order) and emits an
Open event for each established connection. Direction = in if its local port
has a listening socket, else out.

Requires elevation — see §6.

**Watchdog:** a real-time session can lose events when its buffers overflow,
and it outlives a crashed agent. At start the agent stops any leftover session
with the same name (`stop_trace_by_name`), otherwise starting again fails. The
trace is processed on its own thread (`UserTrace::start` +
`process_from_handle`, which returns when the session ends); when that thread
returns — with or without an error — the agent restarts the session with a
backoff of 1 s doubling to 60 s (reset to 1 s after a session has run 5 min),
re-reads the TCP table, and counts the restart. Event loss is read every 60 s
with `ControlTraceW(EVENT_TRACE_CONTROL_QUERY)` (`EventsLost`,
`RealTimeBuffersLost`), because `ferrisetw` only reports events handled.
Counters go to `meta` and into every export manifest, so "every connection" is
best-effort with loss visible, not silently assumed complete.

**UDP rate cap (fallback, off by default):** the owner chose one row per
datagram. `udp_rate_cap` (max rows per second per (PID, remote IP, remote port);
0 = off) stays as the documented lever if measured volume is too high.
Datagrams skipped by the cap are counted as `udp_capped`, separately from ETW
loss.

### 4.2 PID → process name

For each PID not in the cache: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`
+ `QueryFullProcessImageNameW`. Results are cached by PID. When Kernel-Process
reports a process start for a PID, that PID's entry is dropped — the effect of a
PID + creation-time key without a system call per event. The image path from
start/stop events is kept as a fallback for processes that exit before they
can be opened (that path may be in kernel form, `\Device\HarddiskVolumeN\…`;
the process name — the file name — is the same either way). Failed lookups are
not cached; with no fallback the name is `unknown`. The executable path is
stored; command lines are **not** collected.

### 4.3 Foreground app tracking

Hooks, all `WINEVENT_OUTOFCONTEXT`, on one agent thread that runs a message
loop:

- `EVENT_SYSTEM_FOREGROUND` — the foreground window changed.
- `EVENT_OBJECT_NAMECHANGE`, registered only for the current foreground process
  (re-registered on every foreground change) and filtered to the foreground
  window itself (`OBJID_WINDOW`, `CHILDID_SELF`). This catches browser-tab and
  document switches, which do not change the foreground window.
- A hidden top-level window (not message-only: message-only windows do not
  receive the broadcast sleep notice) receives `WM_WTSSESSION_CHANGE`
  (lock/unlock, via `WTSRegisterSessionNotification`), `WM_POWERBROADCAST`
  (`PBT_APMSUSPEND`, `PBT_APMRESUMEAUTOMATIC`), and display on/off
  (`RegisterPowerSettingNotification` for `GUID_CONSOLE_DISPLAY_STATE`:
  0 = off, 1 = on, 2 = dimmed, treated as on).

From the HWND we get the PID → process name (same cache) and the window title.

**Session rules (owner decisions):**

- A session is open only while the PC is *active*: not locked, not asleep,
  display not off, collection not paused. Each condition is tracked
  separately; when the last one clears, a session opens for the current
  foreground app.
- A session ends when the foreground moves to a different process, or when any
  of those conditions starts. Idle without lock does **not** end a session.
  So "3 h in VS Code" means time the app was actually visible and unlocked.
- The Windows lock screen app (`LockApp.exe`) never gets a session.
- `app_sessions.window_title` is the title when the session opened. Every title
  change is logged to `window_titles` with its time. A title is logged once it
  has stayed at least 2 s (this filters titles that count up, like
  "Downloading 41%"); identical consecutive titles are logged once.
  `capture_window_titles = false` stores empty titles and no title log.
- Crash safety: the open session is saved to `meta.open_session` every 60 s.
  On start, a leftover one is written as a session ending at its last save, so
  a crash or power cut loses at most 60 s.

### 4.4 Raw rows (owner decisions)

- **TCP: one row per connection**, open → close. `ts_first` = open, `ts_last` =
  close.
- **UDP: one row per datagram** (owner decision 2026-10-22), `ts_first = ts_last`.

No other merging. `raw_mode` and `merge_window_secs` from earlier drafts are
removed.

Every connection event carries a kind: **Open** or **Close** (TCP) or
**Packet** (UDP). The writer keeps an in-memory map of open TCP connections,
keyed by (local port, remote IP, remote port) → row id:

- Open inserts a row and adds the key (a second Open for a key already in the
  map — e.g. the start-up snapshot plus a live event — is ignored).
- Close updates `ts_last` and removes the key.
- A Close with no key (opened before collection and missed by the snapshot)
  inserts a row with `ts_first = ts_last`; its direction is guessed from the
  local port (49152–65535 ⇒ out, else in).
- Keys with no Close after 24 h are dropped (lost close events), so the map is
  bounded by live connections.

The PID is not part of the key, so a close reported in another process context
still matches; the 4-tuple is unique among live TCP connections.

### 4.5 Service names

Two sources fill `hostnames(ip → hostname, source)`:

1. **Windows DNS client answers** (`capture_dns_names = true`). DNS-Client
   event 3008 gives `QueryName`, `QueryStatus` and `QueryResults` (for example
   `type:  5 github.com;::ffff:140.82.112.3;`). For successful queries
   (`QueryStatus = 0`), IPs are parsed from `QueryResults` (split on `;`, skip
   `type:` entries, strip `::ffff:`) and kept in memory as IP → most recent
   name, capped at 50,000 entries (oldest dropped). `QueryName` is the name the
   app asked for (e.g. `api.github.com`), even through CNAME chains. Reading
   these events makes no network traffic.
2. **Reverse DNS fallback** (`resolve_dns = true`). One background worker does
   PTR lookups through the system resolver for IPs no DNS answer named. A
   failed lookup is stored as `none` and retried after 24 h, so un-nameable IPs
   cannot block new ones. A PTR query goes to the configured DNS server (usually
   the router/ISP) and contains only an IP the PC already connected to.

When the writer stores a connection to an IP with no `hostnames` row, it
inserts one: `source = 'dns'` with the name if the in-memory map has it, else
`'pending'` for the PTR worker. A later DNS answer for that IP upgrades it to
`dns`. DNS answers for IPs never seen in a connection are not stored.

Accepted limits: an IP shared by many names (CDNs) shows the most recent name
looked up; apps using their own DNS-over-HTTPS bypass the Windows DNS client,
so their IPs fall back to reverse DNS. Reverse DNS often returns provider names
(`*.1e100.net`, `lb-…-iad.github.com`) rather than the service name.

### 4.6 Storage: SQLite (WAL)

Single file, WAL mode so the dashboard can read while the agent writes.
Location: **`%LOCALAPPDATA%\PC-Watcher\data.db`** — deliberately not on the
Desktop, because the user's Desktop is OneDrive-synced and a live SQLite file
inside a sync client causes lock conflicts.

Schema (v1). All timestamps are Unix epoch milliseconds (UTC).

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
  process_name TEXT, window_title TEXT      -- title when the session opened
)
window_titles(
  id INTEGER PK, ts INT, process_name TEXT, title TEXT
)
hostnames(
  ip TEXT PK, hostname TEXT NULL,
  source TEXT,                -- 'dns' | 'ptr' | 'none' | 'pending'
  resolved_at INT
)
meta(key TEXT PK, value TEXT)  -- schema version, export watermarks, counters,
                               -- open session
```

Indexes: `connections(process_name, ts_first)`, `connections(remote_ip)`,
`connections(ts_first)`, `app_sessions(process_name, ts_start)`,
`window_titles(ts)`, `window_titles(process_name, ts)`, `hostnames(source)`.

**Estimated footprint:** TCP rows, sessions and titles: ~15–60 MB/day.
Per-datagram UDP adds roughly 150–250 bytes per datagram (row + indexes). A
video call at ~100 datagrams/s adds about 50–90 MB per hour; QUIC video
streaming can reach several hundred datagrams/s, about 200–500 MB per hour. A
day with 2–3 hours of calls or streaming is roughly 0.5–2 GB, so 10-day
retention can reach tens of GB. These are estimates: the ETW smoke test (plan
Task 11) measures the real rate on this PC and records it in the README. If it
is too large, `udp_rate_cap` is the lever.

**Low-disk guard:** when free space on the database drive falls below
`min_free_disk_gb` (default 2), the agent stops writing UDP datagram rows (TCP
rows, sessions and titles continue), counts skipped datagrams as
`udp_skipped_low_disk`, and shows a tray warning. If a write still fails with
"disk full", that batch is dropped and counted as `rows_dropped_disk_full`,
and the agent keeps running.

### 4.7 Retention — unexported data is never deleted

Each successful export records the highest exported row id per table
(`meta.export_wm_connections`, `export_wm_sessions`, `export_wm_titles`) and
`meta.last_export` (time). The hourly pruner deletes a row only when **both**
hold: its id ≤ that table's watermark, and its timestamp is older than
`retention_days` (default 10). Rows written after an export — even with older
timestamps, such as rows still in the 1 s write batch when the export ran —
have higher ids and wait for the next export. No export yet ⇒ nothing deleted.
A failed weekly export (sick Drive client, machine off for two weeks) can grow
the DB past the target size, but it cannot lose data.

Deletes run in chunks of 50,000 rows per transaction so writes are never
blocked for long. `hostnames` is never pruned.

Known limit: a TCP connection still open at export time is exported with its
`ts_last` at that moment; its later close is not re-exported.

## 5. The Two Binaries

| Binary | Lifetime | Role |
|---|---|---|
| `pcw-agent.exe` | Always (tray icon, auto-start at logon) | ETW collector + watchdog, foreground tracker, DNS names, reverse-DNS worker, writer, pruner, weekly exporter |
| `pcw-dash.exe` | On demand | Opens the SQLite file read-only, serves the dashboard at `http://127.0.0.1:7411`, opens the browser, exits on Stop or after 60 min with no requests |

Splitting them is what keeps the footprint promise honest: when the user is not
looking, no web server, no UI framework, nothing extra is resident.

Both binaries are GUI-subsystem (`#![windows_subsystem = "windows"]`), so no
console window appears. The agent writes a log to
`%LOCALAPPDATA%\PC-Watcher\agent.log` (rotated at 5 MB, one old copy kept).
Command-line flags (`--install`, `--uninstall`) attach to the calling console
to print their result.

The agent runs elevated, so anything it starts is elevated too. It opens URLs
and folders with `explorer.exe <target>`, so the browser runs as the normal
user. If the dashboard port is already in use, `pcw-dash` assumes a dashboard is
running, opens the browser to it, and exits.

### 5.1 Agent tray menu

- Open Dashboard (launches `pcw-dash.exe`)
- Pause / Resume collection — pauses everything: network, DNS names, foreground
  sessions and titles. Pausing closes the open session.
- Export now
- Open data folder
- Exit

The tooltip shows: collecting or paused, last export date, and warnings
(export overdue more than 8 days, low disk). Because the agent is elevated,
it allows the `TaskbarCreated` message through Windows' integrity filter
(`ChangeWindowMessageFilter`) so its icon comes back after Explorer restarts.

### 5.2 Dashboard pages (single embedded HTML file, vanilla JS, offline)

No CDN, no npm build. One HTML file compiled into the binary
(`include_str!`), hand-rolled SVG charts so it works with no internet.

- **Overview** — today's totals, top apps by connections, top services, hourly
  activity chart.
- **App drill-down** — pick an app → destinations with counts/time ranges,
  foreground usage (sessions, total time, window titles), event list.
- **Service drill-down** — pick a hostname/IP → which apps talked to it and when.
- **Usage** — foreground sessions timeline with the title log, totals per app.
- Filters: date range, app, protocol, port, direction — applied by the server
  as query parameters.
- A Stop button that ends `pcw-dash`.

## 6. Privileges and Auto-Start

Enabling ETW sessions for kernel providers requires elevation. Foreground-window
hooks must run inside the user's desktop session (a Windows service runs in
session 0 and cannot see user windows — services are ruled out).

**Confirmed:** the owner's account **is** a local administrator under default
UAC, and this PC is a laptop (battery present). A scheduled task at logon that
runs with highest privileges starts the agent elevated in the user session with
no UAC prompt and no stored password.

`pcw-agent.exe --install` (one-time elevated run, accepted):

1. Checks it is elevated; otherwise exits with a clear message.
2. Ends a running agent (`schtasks /End /TN PC-Watcher`) and copies
   `pcw-agent.exe` and `pcw-dash.exe` to `%LOCALAPPDATA%\PC-Watcher\bin\`. The
   task never points into the build folder or OneDrive.
3. Registers the task from an XML definition
   (`schtasks /Create /TN PC-Watcher /XML <file> /F`; the file is UTF-16 LE with
   BOM). Settings that differ from `schtasks` defaults are required:
   - logon trigger for the installing user, `InteractiveToken`,
     `RunLevel = HighestAvailable`
   - `DisallowStartIfOnBatteries = false`, `StopIfGoingOnBatteries = false`
     (defaults are true: the agent would not start, or would be stopped, on
     battery)
   - `ExecutionTimeLimit = PT0S` (the default PT72H kills the agent after 3 days)
   - `MultipleInstancesPolicy = IgnoreNew`; `Priority = 5` (the default 7 is
     below-normal priority, which makes ETW event loss more likely);
     `RestartOnFailure` every 1 min, 3 times
4. Starts the task immediately (`schtasks /Run /TN PC-Watcher`).

`pcw-agent.exe --uninstall` ends and deletes the task; data, config and log
stay in place.

## 7. Export to Google Drive

The user's Google Drive for Desktop client syncs a local folder. The agent
writes structured files there; Drive handles upload. No API keys, no OAuth.

```
<export_folder>/               e.g. G:\My Drive\PC-Watcher\
└── 2026-10-21/                one folder per export date (local time; a second
    │                          export the same day gets "-2", "-3", …)
    ├── connections.csv        every connection row since the last export
    ├── app-usage.csv          foreground sessions (title at session start)
    ├── window-titles.csv      every logged title change
    ├── summary.json           totals, top apps, top services, range
    └── manifest.json          row counts, time range, generated_at,
                               schema version, tool version, counters
```

Schedule: weekly (configurable). Also manual via tray "Export now". Exports
are incremental by row-ID watermark (§4.7); the first export ships full
history. Files are written into `<date>.partial` and renamed when complete.
The export reads through its own read-only connection and streams rows into
the CSV files, so memory stays flat; it holds the writer's lock only to update
the watermarks. With per-datagram UDP, `connections.csv` can be hundreds of MB
to several GB per week; Drive uploads it as-is.

`manifest.json` carries the counters `etw_events_lost`, `etw_buffers_lost`,
`watchdog_restarts`, `udp_capped`, `udp_skipped_low_disk` and
`rows_dropped_disk_full`, so archived data carries its own accuracy caveat.

**Export folder check:** Google Drive for Desktop is installed on this PC, but
no `G:` drive was mounted at review time, and Drive's drive letter may not be
visible to an elevated process. The first install smoke test checks this. If
the elevated agent cannot see it, set `export_folder` to a local folder (for
example `%USERPROFILE%\PC-Watcher-Export`) and add that folder in Drive for
Desktop under "Folders from your computer". When the export folder is missing,
the export fails, is retried hourly, the tray warns, and nothing unexported is
pruned.

## 8. Config

`%LOCALAPPDATA%\PC-Watcher\config.toml`, created on first run with defaults.
Missing keys take their defaults.

```toml
retention_days = 10
resolve_dns = true              # reverse-DNS fallback (makes PTR queries)
capture_dns_names = true        # names from Windows DNS client answers (no traffic)
capture_window_titles = true    # owner decision: on
export_folder = "G:\\My Drive\\PC-Watcher"
export_schedule = "weekly"
dashboard_port = 7411
udp_rate_cap = 0                # 0 = off; thinning fallback, §4.1
min_free_disk_gb = 2            # below this, UDP datagram rows stop, §4.6
```

## 9. Resolved Decisions

| Question | Decision |
|---|---|
| Admin account | Yes — logon task with highest privileges elevates silently |
| Laptop | Yes — task must run on battery and without a time limit |
| TCP rows | **Raw**, one row per connection (open → close) |
| UDP rows | **One row per datagram** (2026-10-22); rate cap off, documented fallback |
| Window titles | **Captured**; every change logged with its time (2026-10-22) |
| Session end | App-switch, lock, sleep, hibernate, display-off, pause; idle does not end; lock screen never counted |
| Service names | Windows DNS client answers first, reverse DNS fallback (2026-10-22) |
| Unexported data | Pruner never deletes rows above the export watermark |
| ETW reliability | Watchdog + restart + start-up connection snapshot + loss counters |
| ETW provider | Microsoft-Windows-Kernel-Network (+ DNS-Client, Kernel-Process) via ferrisetw |

Still open (config value, not design): the Google Drive export folder path on
this machine — `G:` was not mounted at review time (§7).

## 10. Constraints

- Windows 10/11 x64. Rust stable toolchain. Confirmed on this PC
  (2026-10-22): Rust 1.99.0 stable `x86_64-pc-windows-msvc`, Visual Studio
  Build Tools 2022 with Windows SDK 10.0.26100, `rust-lld` in the toolchain.
- Local administrator account (confirmed). Laptop with battery (confirmed).
- Google Drive for Desktop installed (confirmed); signed in and mounted — not
  confirmed (§7). The app itself never talks to Google.
- The only network traffic the app itself generates is optional reverse-DNS
  (PTR) lookups through the system resolver.
- OneDrive-synced Desktop exists on this machine — the DB must stay in
  `%LOCALAPPDATA%`, never inside a sync root. The project folder itself is
  currently under the OneDrive Desktop: before building, move it out (for
  example `C:\dev\PC-Watcher`), or at least set the `CARGO_TARGET_DIR`
  environment variable to a folder outside OneDrive. `target/` grows to several
  GB and OneDrive locks files while the linker writes them.

## 11. Build-Performance Strategy (from the r/rust thread)

The referenced thread's consensus, applied from day one:

1. **Cargo workspace of small crates**, two binaries, everything else
   libraries. Shallow dependency graph between crates (no deep chains) so
   compilation parallelizes.
2. **`[workspace.dependencies]`** at the root; members use
   `dep = { workspace = true }` (members may add features). One pinned version
   of every direct dependency.
3. **Fast linker:** `rust-lld` (ships with the toolchain), set in
   `.cargo/config.toml` as `linker = "rust-lld"` for
   `x86_64-pc-windows-msvc` — verified to link on this PC.
4. **Isolate the heavy dependency:** our code imports the `windows` crate only
   in `pcw-etw` and `pcw-foreground`, each with an explicit feature list.
   `ferrisetw` brings its own `windows` 0.57 and `tao`/`tray-icon` bring
   `windows-sys`, so a clean build compiles more than one version; this is
   unavoidable and costs time only on clean builds, since Cargo caches compiled
   dependencies.
5. **Dev profile:** incremental on (default); optional Cranelift backend on
   nightly for very fast debug builds — documented, not required.
6. **Release profile:** `lto = "thin"`, `opt-level = 3`, `strip = true`.
7. `cargo build --timings` documented in the README for bottleneck hunting.

### Crate layout

```
pc-watcher/
├── Cargo.toml              # workspace + [workspace.dependencies]
├── .cargo/config.toml      # rust-lld linker
├── crates/
│   ├── pcw-types           # shared structs + process cache, zero heavy deps
│   ├── pcw-config          # TOML config load/save, data dir
│   ├── pcw-storage         # SQLite schema, writes, queries, pruning
│   ├── pcw-etw             # ETW session + watchdog, TCP table snapshot,
│   │                       # process lookup   (ferrisetw + windows crate)
│   ├── pcw-foreground      # session tracker, WinEvent + lock/power hooks,
│   │                       # small Win32 helpers for the agent (windows crate)
│   ├── pcw-resolve         # reverse-DNS worker
│   ├── pcw-export          # CSV/JSON weekly export
│   ├── pcw-dashboard       # localhost server + embedded UI
│   ├── pcw-agent           # bin (+ small lib for tests): tray, wiring, timers
│   └── pcw-dash            # bin: on-demand dashboard
```

## 12. Performance Targets

- Agent idle: < 40 MB RAM, < 1% CPU average.
- Under traffic: measured, not promised. The ETW smoke test records events per
  second, CPU and DB growth during a 10-minute video call; the README keeps the
  numbers.
- ETW buffers: 64 KB buffers, `ferrisetw` defaults otherwise; raise if the loss
  counters show drops. Batched DB inserts: one transaction per flush, every 1 s
  or 500 events, whichever comes first — never one transaction per event.
- Dashboard cold start to browser paint: < 3 s on 10 days of data (queries are
  indexed; no full scans on the hot pages). If the Overview misses this with
  per-datagram UDP volume, the documented fallback is an hourly roll-up table
  kept by the writer (not in v1).
- DB file size: see §4.6. Export-guarded pruning may temporarily exceed it.

## 13. Testing Notes (for the planner)

- Storage: temp-file SQLite integration tests (insert, TCP lifecycle, queries,
  watermark prune, disk full simulated with `PRAGMA max_page_count`).
- Session tracker: pure state-machine tests (lock / sleep / display / pause
  conditions, lock screen app, 2 s title rule).
- ETW decoding: pure-function tests (port byte order, event → kind mapping,
  DNS result parsing, snapshot direction).
- ETW/foreground/lock hooks: thin OS adapters behind pure logic; a manual smoke
  checklist covers the OS edge.
- Export: golden-file tests on CSV/JSON output, including hostile window
  titles, manifest counters, and rows that arrive after an export.
- Service names: fake resolver; failed lookups remembered; no starvation.
- Install: the task XML is produced by a pure function and its settings are
  asserted.
- Watchdog: fault-injection test that kills the ETW session and asserts
  restart + counter increment (manual or semi-automated).
