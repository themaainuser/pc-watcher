//! End-to-end smoke example (manual, elevated): stops any leftover
//! `PC-Watcher-Net` session, prints the connection snapshot, then runs a live
//! session for 20 s and prints every collected event, the queried loss, and
//! stops the session.
//!
//! Run from an elevated PowerShell:
//!   cargo run -p pcw-etw --example capture

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pcw_etw::SESSION_NAME;
use pcw_etw::process::query_process;
use pcw_etw::snapshot::tcp_snapshot_events;
use pcw_etw::source::{SourceConfig, SourceError, query_loss, run_session, stop_session};
use pcw_types::{Counters, ProcessCache, SharedProcessCache};

const RUN_SECS: u64 = 20;

fn main() {
    // A crash or previous run can leave the session alive; starting over a
    // leftover name would fail.
    stop_session(SESSION_NAME);

    let cache: SharedProcessCache =
        Arc::new(Mutex::new(ProcessCache::new(Box::new(query_process))));
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .expect("clock before unix epoch");

    let snapshot = tcp_snapshot_events(&cache, now_ms);
    println!(
        "=== startup snapshot: {} established connections ===",
        snapshot.len()
    );
    for event in &snapshot {
        println!("{event:?}");
    }

    let (tx, rx) = mpsc::channel();
    let cfg = SourceConfig {
        capture_dns: true,
        udp_rate_cap: 0,
    };
    let counters = Arc::new(Counters::default());
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let runner_cache = Arc::clone(&cache);
    let runner_counters = Arc::clone(&counters);
    let runner_paused = Arc::clone(&paused);
    let runner = std::thread::spawn(move || {
        run_session(&cfg, runner_cache, runner_counters, runner_paused, tx)
    });

    println!("=== listening to {SESSION_NAME} for {RUN_SECS} s (browse around to see events) ===");
    let deadline = Instant::now() + Duration::from_secs(RUN_SECS);
    let mut printed = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(event) => {
                println!("{event:?}");
                printed += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    println!("=== collected {printed} events in {RUN_SECS} s ===");

    // Must be queried while the session is still alive.
    match query_loss(SESSION_NAME) {
        Some((events_lost, buffers_lost)) => {
            println!("=== query_loss: events_lost={events_lost} buffers_lost={buffers_lost} ===")
        }
        None => println!("=== query_loss: unavailable (elevation or session gone) ==="),
    }

    // Stopping makes `process_from_handle` (and the runner thread) return.
    stop_session(SESSION_NAME);
    match runner.join() {
        Ok(Ok(())) => println!("=== run_session returned: session ended cleanly ==="),
        Ok(Err(SourceError::Trace(e))) => println!("=== run_session returned (trace): {e} ==="),
        Ok(Err(SourceError::Win32(e))) => println!("=== run_session returned (win32): {e} ==="),
        Err(_) => println!("=== runner thread panicked ==="),
    }

    let counters: Vec<(&'static str, u64)> = counters.snapshot().into_iter().collect();
    println!("=== counters: {counters:?} ===");
}
