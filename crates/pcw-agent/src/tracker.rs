//! The tracker thread's loop (plan Task 13): session signals in, writer
//! messages out. Extracted from the binary (`main.rs` used to own it) so the
//! stop path is testable without a tray: the loop terminates either on an
//! explicit stop flag (the deterministic Exit trigger) or, as a secondary
//! trigger, when every signal sender is gone — and `finalize` runs exactly
//! once in both cases (review flag F12).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pcw_foreground::{SessionSignal, SessionTracker, TrackerOutput};
use pcw_storage::WriteOp;

use crate::writer::WriterMsg;

/// Incremental stop-flag check despite any heartbeat size: the tracker notices
/// a shutdown within at most this slice, matching main's fast exit response.
const POLL: Duration = Duration::from_millis(200);

/// Epoch milliseconds (0 only before the epoch; timestamps are epoch-ms).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Owns the tracker: drains session signals, posts tracker outputs as
/// `WriterMsg::Op` and a heartbeat `WriterMsg::OpenSession` every `heartbeat`.
///
/// The loop RETURNS as soon as `stop` is set — main raises the flag right
/// after `hooks.stop()` (shifting the disconnect out of the stop role: the
/// tray pause-menu closure keeps a sender clone alive forever, so a
/// sender-disconnect would never arrive and stop sender-based shutdown).
/// After the loop, `finalize` runs EXACTLY ONCE (F12): the final outputs,
/// then `WriterMsg::OpenSession {None}` (the writer clears meta.open_session)
/// and `WriterMsg::Shutdown` (the writer flushes its queue and exits).
///
/// All signal senders dropped is kept as a SECONDARY trigger (tracker
/// panic-safety when main itself is gone); it cannot fire while a menu
/// closure holds a sender clone.
pub fn tracker_loop(
    rx: Receiver<SessionSignal>,
    tx_writer: Sender<WriterMsg>,
    capture_titles: bool,
    heartbeat: Duration,
    stop: Arc<AtomicBool>,
) {
    let mut tracker = SessionTracker::new(capture_titles);
    let mut last_heartbeat = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(POLL) {
            Ok(signal) => {
                for out in tracker.on_signal(signal) {
                    if let Err(e) = tx_writer.send(tracker_op(out)) {
                        // The writer is gone; nothing can be finalized into.
                        log::error!("writer channel closed: {e}");
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // Secondary stop trigger: every signal source is gone.
            Err(RecvTimeoutError::Disconnected) => {
                log::info!("tracker: all signal senders gone; finalizing");
                break;
            }
        }
        if last_heartbeat.elapsed() >= heartbeat {
            last_heartbeat = Instant::now();
            let _ = tx_writer.send(WriterMsg::OpenSession {
                session: tracker.open_session(),
                now_ms: now_ms(),
            });
        }
    }
    // Exactly once (F12), for BOTH triggers: the loop's only exits are the
    // two breaks above (the early `return` is the writer-gone case, where
    // nothing can be sent anyway). The final tracker outputs, then
    // `OpenSession {None}` and `Shutdown` — see the function doc.
    let ts = now_ms();
    for out in tracker.finalize(ts) {
        let _ = tx_writer.send(tracker_op(out));
    }
    // `finalize` above closed the session, so this heartbeat is `None` and
    // clears meta.open_session (no stale crash marker).
    let _ = tx_writer.send(WriterMsg::OpenSession {
        session: tracker.open_session(),
        now_ms: now_ms(),
    });
    let _ = tx_writer.send(WriterMsg::Shutdown);
}

/// Tracker outputs → the writer's messages.
fn tracker_op(out: TrackerOutput) -> WriterMsg {
    match out {
        TrackerOutput::Session(session) => WriterMsg::Op(WriteOp::Session(session)),
        TrackerOutput::Title(title) => WriterMsg::Op(WriteOp::Title(title)),
    }
}
