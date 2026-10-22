//! Manual smoke harness for the Win32 hooks. Needs an interactive desktop
//! session (a human at the screen); run with
//! `cargo test -p pcw-foreground --test os_smoke -- --ignored --nocapture`.
//!
//! Expectations (90 s window):
//! - alt-tab between apps → `Foreground` with process name and title;
//! - switching browser tabs / documents in the focused app → `Title`;
//! - Win+L → `Locked`, then `Unlocked` after sign-in;
//! - display off (short `powercfg` timeout or lid close with "do nothing")
//!   → `DisplayOff`, then `DisplayOn` when it wakes;
//! - idle at the desktop → no signals at all.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pcw_foreground::SessionSignal;
use pcw_foreground::os::start_hooks;
use pcw_types::{ProcessCache, SharedProcessCache};

/// A cache without a query callback: process names come from events the
/// agent records; this smoke test only prints whatever resolves.
fn fresh_cache() -> SharedProcessCache {
    Arc::new(Mutex::new(ProcessCache::new(Box::new(|_| None))))
}

#[test]
#[ignore = "manual: needs interactive session"]
fn prints_foreground_signals_for_90_seconds() {
    let (tx, rx) = std::sync::mpsc::channel::<SessionSignal>();
    let handle = start_hooks(tx, fresh_cache(), true)
        .expect("hooks must start in an interactive desktop session");
    println!("hooks running; interact with the desktop for 90 s…");

    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(signal) => println!("{signal:?}"),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    handle.stop();
    println!("hooks stopped");
}
