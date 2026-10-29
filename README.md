# PC-Watcher

A Windows tray application (Rust) that records per-process network
connection metadata into SQLite, with an on-demand localhost dashboard.
Started by hand: it runs only while it is open (no auto-start).

## Workspace layout

Cargo workspace with ten member crates under `crates/`:

- Libraries: `pcw-types`, `pcw-config`, `pcw-storage`, `pcw-export`,
  `pcw-resolve`, `pcw-etw`, `pcw-foreground`, `pcw-dashboard`
- Binaries: `pcw-agent`, `pcw-dash`

All dependency versions are pinned once in the root `Cargo.toml`
(`[workspace.dependencies]`); member crates reference them with
`dep = { workspace = true }`.

## Build

```powershell
cargo build --release
```

The release profile uses `lto = "thin"`, `opt-level = 3`, and `strip = true`
(see root `Cargo.toml`); the dev profile enables `incremental = true`.

## Build profile / timing

To profile compile times across the workspace:

```powershell
cargo build --timings
# then open target/cargo-timings/cargo-timing.html in a browser
```

## Linker configuration

The host target (`x86_64-pc-windows-msvc`) is configured to use `rust-lld`,
which ships inside the rustup toolchain sysroot (no external LLVM install
needed). See `.cargo/config.toml`:

```toml
[target.x86_64-pc-windows-msvc]
linker = "rust-lld"
```

No external `lld-link` install is required; `rust-lld` acts as the MSVC
`link.exe` compatible driver here.

## Quick start (new machine)

Prerequisites: Windows 10/11, a Rust toolchain
([rustup](https://rustup.rs) — no external linker is needed; the build
uses `rust-lld` from the toolchain), and an account that can approve UAC
prompts (capture reads the Windows ETW kernel session, which needs an
admin token).

1. Get the source. The project is source-only — there are no published
   binaries — and it must sit in a plain local folder (see the OneDrive
   rule below).
2. Build: `cargo build --release`.
3. Start the agent: double-click `target\release\pcw-agent.exe` (or run it
   from a terminal). A UAC prompt appears **by design** — confirm it and
   the tray icon shows up. There is no install step and no auto-start: the
   agent runs only while you keep it running. **Exit** in the tray stops
   it; if it crashes, nothing restarts it.
4. Double-clicking the exe again while it runs shows "PC-Watcher is
   already running" instead of starting a second copy (a duplicate would
   break the first one's capture).
5. After a few minutes of normal use, tray → **Open Dashboard**: the
   browser opens `http://127.0.0.1:7411/` with what has been recorded so
   far. Exports (pill or scheduled) write to `export_folder` (see below).

Data lands in `%LOCALAPPDATA%\PC-Watcher\`: `data.db` (SQLite, WAL),
`config.toml`, `agent.log` (rotated at 5 MB, one old copy kept). To remove
the app completely: tray **Exit**, then delete that folder.

**Migrating from an older build** (one that registered the `PC-Watcher`
scheduled task): once, from an elevated terminal, run
`schtasks /Delete /TN PC-Watcher /F` and delete
`%LOCALAPPDATA%\PC-Watcher\bin\`; data, config and log stay in place.

## Configuration reference

`%LOCALAPPDATA%\PC-Watcher\config.toml`, created on first run with these
defaults. Missing keys take their defaults (spec §8, transcribed — the one
difference is `export_folder`: the historical spec §8 still records the
original Drive-path default, while the shipped default is the local folder
`D:\PC-Watcher`):

```toml
retention_days = 10
resolve_dns = true              # reverse-DNS fallback (makes PTR queries)
capture_dns_names = true        # names from Windows DNS client answers (no traffic)
capture_window_titles = true    # owner decision: on
export_folder = "D:\\PC-Watcher"
export_schedule = "weekly"
dashboard_port = 7411
udp_rate_cap = 0                # 0 = off; thinning fallback, §4.1
min_free_disk_gb = 2            # below this, UDP datagram rows stop, §4.6
```

`export_schedule` accepts `daily` (1 day), `weekly` (7 days), `monthly`
(30 days), or a plain integer number of days (for example `14`). Anything
else falls back to weekly and is logged at startup; the tray's
`export overdue` warning follows the same period (one day beyond it).

## Dashboard usage

- `pcw-dash.exe` is on-demand: start it from the tray
  (**Open Dashboard**), by double-click, or as
  `pcw-dash.exe [--port N]`.
- The port defaults to `dashboard_port` (7411); `--port N` overrides it
  (an invalid value falls back to the config default).
- It serves `http://127.0.0.1:<port>/` — **loopback only**, never reachable
  from the LAN (spec §5.2).
- On start it opens the browser via `explorer.exe` so the browser runs as
  the normal user, even when the agent tray (elevated) spawned it.
- If the port is already in use, it assumes a dashboard is already serving
  there, opens the browser to it, and exits 0.
- The UI's **Stop** button (`POST /api/shutdown`) ends `pcw-dash`; it also
  exits after 60 minutes with no requests (spec §5).
- The **Export** pill downloads the *current view*: an Excel `.xlsx`
  workbook (sheets `Manifest`, `Connections`, `Sessions`, `Titles`) or a
  CSV `.zip` bundle (`manifest.json` plus one CSV per dataset), with full
  row-level detail — every column the database holds — and a manifest
  naming the filters, time range and row counts. Exports follow the filter
  bar and the active drill-down: on an app drill-down they are scoped to
  that app; on a host drill-down the export stays range-wide and the menu
  says so.

## Export to disk (optional Drive sync)

Exports stream from `data.db` into `<export_folder>\<date>\`
(`connections.csv`, `app-usage.csv`, `window-titles.csv`, `summary.json`,
`manifest.json`) with per-row watermark for incremental exports (spec §7);
scheduled weekly or from the tray (**Export now**). Dated subfolders with a
repeat `"-2"`, `"-3"` suffix per day. The default `export_folder` is the
local folder `D:\PC-Watcher`; to land scheduled exports in Google Drive
instead, point `export_folder` at a Drive-mounted folder, or at any local
folder synced into Drive through "Folders from your computer".

**Export-folder fallback (spec §7):** an elevated process may not see a
Drive-mounted folder. If the elevated agent cannot see the folder, set
`export_folder` to a local folder (for example
`%USERPROFILE%\PC-Watcher-Export`) and add that folder in Drive for Desktop
under "Folders from your computer". When the export folder is missing, the
export fails, is retried hourly, the tray warns, and nothing unexported is
pruned.

## Measured volume (spec §4.6, §12)

**Pending the owner's manual run** (Task 11 smoke Step 7c): a 10-minute
video call with the ETW session live, recording events/s UDP, example CPU
and DB growth. No numbers are published here until measured.

## Smoke checklists (manual, owner-run — agents never run these)

Automated tests cannot lock a screen, sleep the machine or unplug a
charger, so these stay manual. They can be run once after the first start.

### Foreground hooks (Task 10)

```powershell
cargo test -p pcw-foreground --test os_smoke -- --ignored --nocapture
```

Then, during the 90-second window it prints signals for:

- [ ] Alt-tab between two apps → a `Foreground` signal with the process name
      and window title for each switch.
- [ ] Switch browser tabs (or documents) in the focused app → a `Title`
      signal for each tab.
- [ ] Win+L → `Locked`; after sign-in → `Unlocked`.
- [ ] Turn the display off (short `powercfg` timeout, or closing the lid with
      "do nothing" as the lid action) → `DisplayOff`; when the display turns
      back on → `DisplayOn`.
- [ ] Idle at the desktop (no interaction) → no signals at all.

### ETW capture (Task 11)

- [ ] Elevated `cargo run -p pcw-etw --example capture` while browsing:
      real process names + remote port 443, Close follows Open, DNS events
      carry names; receive/accept events put the other machine's IP in
      `remote_ip`; `query_loss` returns `Some` while elevated.
- [ ] `logman stop PC-Watcher-Net -ets` in a second elevated shell →
      `run_session` returns and the example prints it.
- [ ] Hibernate/resume during a session → record the behavior.

### Agent run & tray (Task 13)

- [ ] Double-click → UAC prompt → tray icon appears, no console window.
- [ ] Second double-click while running → "PC-Watcher is already running",
      no second process.
- [ ] Declining the UAC prompt → explanatory message box, nothing stays
      running.
- [ ] Tray icon with exactly five items: Open Dashboard, Pause/Resume,
      Export now, Open data folder, Exit.
- [ ] Unplug charger → agent keeps running.
- [ ] Kill explorer → tray icon comes back.
- [ ] Task Manager after 10 min idle: memory < 40 MB, CPU < 1%.
- [ ] Pause → `connections` and `app_sessions` stop growing; Resume →
      grow again.
- [ ] Export now → dated folder in `export_folder` (or the local-folder
      fallback above when `G:` is not visible elevated).
- [ ] `agent.log` has start-up lines.
- [ ] Tray **Exit** stops the agent; the data folder remains.

## Full end-to-end acceptance (owner manual — NEVER RUN by agents)

Brief Step 2 items 1–8 for Task 15; overall acceptance of the product:

1. [ ] Start the agent (double-click + UAC) → no console window, tray
       present, idle footprint within spec §12.
2. [ ] Browse for 10 minutes → tray **Open Dashboard** → the browser opens
       as the normal user (Task Manager "Elevated" column = No); Overview
       shows the activity; drill-downs work; Usage shows sessions with the
       title log (tab switches appear).
3. [ ] Filters: protocol UDP, port 443, direction in — each changes the
       results.
4. [ ] Lock 5 min mid-session → Usage excludes it; wake from sleep, leave
       at the lock screen 2 min → no session for that time, no
       `LockApp.exe` session.
5. [ ] **Export now** → the five files in the export folder; titles intact;
       counters present.
6. [ ] Kill `pcw-agent.exe` in Task Manager → it stays dead (no
       auto-restart). Start it again → `agent.log` shows a leftover ETW
       session stopped and the open session recovered.
7. [ ] From another LAN device, `http://<lan-ip>:7411` is unreachable while
       the dashboard runs.
8. [ ] Click **Stop** in the dashboard → `pcw-dash.exe` exits.

## Automated tests

```powershell
cargo test --workspace
```

All suites are green (111 passed + 1 ignored); the one ignored test is the
manual `os_smoke` harness above. Dashboard bind/run/routes semantics are
covered by `cargo test -p pcw-dashboard` (8 tests, incl. loopback-only
bind, `/api/shutdown` stop, and 503 recovery).

## OneDrive rule

The project deliberately lives **outside** any OneDrive / cloud-sync folder
(it currently sits at `D:\PC-Watcher`). Reasons:

- `target/` grows to multi-GB during normal development, and sync clients
  would try to upload every intermediate artifact.
- Sync clients frequently hold file locks on freshly produced binaries,
  which breaks the linker (`error: link failed` / "Access is denied").

If you ever clone or move the project, keep it in a plain local path and
point `CARGO_TARGET_DIR` at a local (non-synced) directory if in doubt.
