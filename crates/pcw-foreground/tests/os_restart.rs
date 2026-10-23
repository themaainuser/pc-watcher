//! Regression test: `start_hooks` must be restartable within one process.
//! The hook thread tears EVERYTHING down after its message loop exits —
//! win-event hooks, session/power registrations, the hidden window and the
//! window class — so a second start/stop cycle in the same process succeeds.
//! Before that teardown existed, the second start died on
//! `RegisterClassW` → `ERROR_CLASS_ALREADY_EXISTS` (1410).

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use pcw_foreground::SessionSignal;
use pcw_foreground::os::start_hooks;
use pcw_types::{ProcessCache, SharedProcessCache};

#[test]
fn start_stop_is_restartable_in_one_process() {
    let cache: SharedProcessCache = Arc::new(Mutex::new(ProcessCache::new(Box::new(|_| None))));
    // Two full cycles back to back, serialized: the second `start_hooks`
    // only succeeds when the first cycle's teardown ran, because the failed
    // class registration would otherwise make it fail permanently.
    for cycle in 1..=2 {
        let (tx, rx) = channel::<SessionSignal>();
        let handle = start_hooks(tx, cache.clone(), false).unwrap_or_else(|e| {
            panic!("start_hooks cycle {cycle} failed: {e}");
        });
        handle.stop();
        // Keep the receiver alive through `stop` (the hook thread may still
        // send), then let it end the cycle.
        drop(rx);
    }
}
