# Plan — on-demand export (CSV bundle + Excel) with detailed metadata

Approved in chat 2026-10-26 (brainstorming playback: export menu in the dashboard filter bar, CSV and Excel, manifests + full row-level columns, exports honor active filters; plus the standing fix that `export_folder` must stop pointing at the nonexistent G: drive and use `D:\PC-Watcher`).

## Context

PC-Watcher: Rust workspace, local-only network/activity monitor. `pcw-dashboard` serves one embedded `ui.html` (vanilla JS, single file) at 127.0.0.1:7411 plus JSON endpoints (`/api/overview`, `/api/app`, `/api/service`, `/api/usage`, `POST /api/shutdown`). Live DB: ~160k connection rows. The agent already has a weekly CSV export task (writes to `export_folder`); this plan adds *on-demand* exports through the dashboard with richer metadata and an Excel option.

## Global Constraints

- UI stays ONE file (`crates/pcw-dashboard/src/ui.html`), vanilla JS, no frameworks/build step. Test-enforced: no `http://`, no `https://`, no `//cdn` anywhere except the SVG namespace string; no `valueAsDate` token; must contain `getFullYear` (local-date construction).
- Same-origin fetch only; dashboard binds 127.0.0.1.
- New dependencies are server-side Cargo crates only. Runtime must not gain any HTTP client or external network access (the no-telemetry positioning is product truth).
- Filter params on export endpoints are the same lowercase `from`/`to`/`app`/`protocol`/`port`/`direction` (ms-epoch from/to, `to` inclusive-day as the UI computes it today) used by the existing endpoints; invalid filter → 400 with the existing error shape.
- `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check` all clean.
- No fabricated measurements in README/docs.
- Historical spec/plan documents under `docs/superpowers/` are records of past decisions — never rewrite them to match new defaults.

## Task 1: config default `export_folder` → `D:\PC-Watcher`

Change the shipped default and its test + user-facing doc:

- `crates/pcw-config/src/lib.rs`: the default template string (line ~20) and the `Default` impl value (line ~61): `G:\My Drive\PC-Watcher` → `D:\PC-Watcher`.
- `crates/pcw-config/tests/config.rs` (~lines 29, 48): expected `PathBuf` updated to match.
- `README.md` (~line 86): config example updated.
- Do NOT touch `docs/superpowers/specs/*` or `docs/superpowers/plans/*` (historical records).
- Gate: `cargo test -p pcw-config` green; workspace unaffected otherwise.

## Task 2: export endpoints in `pcw-dashboard` (server side)

New GET routes returning file downloads, honoring the existing filter parsing:

- `GET /api/export/xlsx` → `.xlsx` workbook, sheets in order: `Manifest`, `Connections`, `Sessions`, `Titles`.
- `GET /api/export/csv` → `.zip` containing exactly `manifest.json`, `connections.csv`, `sessions.csv`, `titles.csv`.
- Both set `Content-Disposition: attachment; filename=pcw-export-<YYYY-MM-DD>.<ext>` and stream from the same DB queries the existing endpoints use (full result sets for the range, NOT the UI's 300-row cap).
- **Row-level detail: every column the DB holds, nothing condensed** — connections: `ts_first_ms`, `ts_last_ms`, `process_name`, `service_name` (if present), `remote_ip`, `remote_port`, `local_port`, `protocol`, `direction`, `hostname`; sessions: `ts_start_ms`, `ts_end_ms`, `process_name`, `window_title`; titles: `ts_ms`, `process_name`, `title`. Columns are discovered from the actual schema — the implementer reads `pcw-store` first; whatever fields exist ship, none invented.
- **Manifest (both formats):** app version (`env!("CARGO_PKG_VERSION")`), export timestamp with local timezone, date range (from/to as ms + readable), every filter value applied (or "any"), row count per dataset, source DB path. In xlsx it is the first sheet as key/value rows; in the zip it is `manifest.json`.
- Suggested crates (implementer may pick equivalents if licensing/API is cleaner): `rust_xlsxwriter` for xlsx, `zip` for the archive, `serde_json` already present for manifest.json. CSV rows hand-rolled or via `csv` crate — proper quoting either way (titles contain commas/quotes).
- Tests in `crates/pcw-dashboard/tests/server.rs` against the existing seeded-DB test harness: 200 + correct content type + `PK` magic for both; zip lists exactly the 4 files; xlsx parses (via `rust_xlsxwriter`'s format being valid — assert with `zip` crate reading `xl/workbook.xml` contains the 4 sheet names); filter honored (export with `app=<seeded>` has only that app's rows — assert on the CSV text); empty-range export still returns valid files with zero rows; manifest contains version + counts; bad filter → 400.
- Memory note: full-range export of ~160k rows is tens of MB in memory — acceptable for a local single-user tool; do not add streaming machinery.

## Task 3: Export menu in `ui.html` (depends on Task 2)

- An "Export" pill button in the filter bar between Apply and Reset, matching the existing pill language (white pill, hairline border, soft shadow; opens a small dropdown with two items: "Excel (.xlsx)" and "CSV bundle (.zip)"; Escape/outside-click closes; keyboard reachable).
- Clicking an item fetches `/api/export/<fmt>?` + the existing `query()` string (so it exports exactly the current view's filters), receives a blob, triggers a download via an `a[download]` + object URL, revokes the URL after. While generating, the menu shows a disabled "Preparing…" state; on failure an inline error line naming the recovery (retry; check agent/dashboard).
- Filename comes from the server's Content-Disposition.
- Keep `ui_has_no_external_urls` and `ui_default_dates_use_local_components` passing untouched. Add a guard test pinning the wiring: `ui.html` contains both `/api/export/xlsx` and `/api/export/csv`.
- Gates: `cargo test -p pcw-dashboard`, workspace suite, clippy, fmt.

## Task 4 (controller, not dispatched): deploy + verify

Rebuild release, swap `%LOCALAPPDATA%\PC-Watcher\bin\pcw-dash.exe` (SHA256-verified), restart, browser round: open Export menu on desktop width, download both formats, open the .xlsx and .zip contents to confirm manifest + sheets, confirm filters respected (filter to one app → export → row check). Detach.
