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
