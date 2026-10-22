# PC-Watcher

An always-on Windows tray application (Rust) that records per-process network
connection metadata into SQLite, with an on-demand localhost dashboard.

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

## Foreground hooks smoke test (manual)

The Win32 hook adapter in `crates/pcw-foreground` needs an interactive desktop
session. Verify it by hand with:

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

## OneDrive rule

The project deliberately lives **outside** any OneDrive / cloud-sync folder
(it currently sits at `D:\PC-Watcher`). Reasons:

- `target/` grows to multi-GB during normal development, and sync clients
  would try to upload every intermediate artifact.
- Sync clients frequently hold file locks on freshly produced binaries,
  which breaks the linker (`error: link failed` / "Access is denied").

If you ever clone or move the project, keep it in a plain local path and
point `CARGO_TARGET_DIR` at a local (non-synced) directory if in doubt.
