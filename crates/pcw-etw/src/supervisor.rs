//! The session watchdog: restart bookkeeping and event-loss accounting.
//!
//! [`Supervisor::on_exit`] is called every time `run_session` returns, no
//! matter whether it ended cleanly or with an error — the session must always
//! keep running. It returns the delay before the next restart: 1 s doubling
//! to a 60 s cap, reset to 1 s after a session has been up for at least 5
//! minutes.[`Supervisor::on_stats`] adds the event loss observed by
//! `ControlTraceW(EVENT_TRACE_CONTROL_QUERY)`, but only the loss that is new
//! since the previous query of the same session. A supervisor built with
//! [`Supervisor::with_totals`] resumes a previous run's lifetime totals.

use std::time::Duration;

use crate::source::SourceError;

/// A session must run for at least this long for the backoff to reset.
const RESET_RUN_SECS: u64 = 5 * 60;
/// Backoff starts here.
const FIRST_WAIT_SECS: u64 = 1;
/// Backoff never exceeds 60 s.
const MAX_WAIT_SECS: u64 = 60;

#[derive(Debug)]
pub struct Supervisor {
    /// How many times `run_session` ended (clean or not) and was restarted.
    pub restarts: u64,
    /// Total `EventsLost` across sessions (only increases within a session).
    pub events_lost: u64,
    /// Total `RealTimeBuffersLost` across sessions (same accounting rule).
    pub buffers_lost: u64,
    backoff_secs: u64,
    /// Per-session loss counters as of the last `on_stats` call; the delta is
    /// added here so a counter that stays flat adds nothing twice.
    baseline_events_lost: u64,
    baseline_buffers_lost: u64,
}

impl Supervisor {
    pub fn new() -> Self {
        Self::with_totals(0, 0, 0)
    }

    /// A supervisor resuming a previous run's lifetime totals (the counters
    /// loaded from `meta` at startup): the cumulative public counters start
    /// from them, while the per-session baselines start at zero because the
    /// next session runs from scratch (the same state a fresh [`Supervisor`]
    /// would be in). Without this, the first timer tick would publish fresh
    /// zeros over the loaded lifetime `etw_events_lost`/`etw_buffers_lost`/
    /// `watchdog_restarts` values.
    pub fn with_totals(restarts: u64, events_lost: u64, buffers_lost: u64) -> Self {
        Self {
            restarts,
            events_lost,
            buffers_lost,
            backoff_secs: FIRST_WAIT_SECS,
            baseline_events_lost: 0,
            baseline_buffers_lost: 0,
        }
    }

    /// A session run ended. Counts a restart (even for a clean exit, because
    /// a clean exit still means the process dropped the session), and
    /// returns how long to wait before starting the next one.
    pub fn on_exit(&mut self, result: Result<(), SourceError>, ran_for: Duration) -> Duration {
        let _ = result;
        self.restarts += 1;
        // Whatever happened to the counters in the old session, the new one
        // starts from zero again.
        self.baseline_events_lost = 0;
        self.baseline_buffers_lost = 0;
        if ran_for.as_secs() >= RESET_RUN_SECS {
            self.backoff_secs = FIRST_WAIT_SECS;
        }
        let wait = Duration::from_secs(self.backoff_secs);
        self.backoff_secs = (self.backoff_secs * 2).min(MAX_WAIT_SECS);
        wait
    }

    /// Records the query result of `ControlTraceW(EVENT_TRACE_CONTROL_QUERY)`
    /// for the current session: only the increase since the last call of this
    /// session is added to the public counters.
    pub fn on_stats(&mut self, session_events_lost: u64, session_buffers_lost: u64) {
        self.events_lost += session_events_lost.saturating_sub(self.baseline_events_lost);
        self.buffers_lost += session_buffers_lost.saturating_sub(self.baseline_buffers_lost);
        self.baseline_events_lost = session_events_lost;
        self.baseline_buffers_lost = session_buffers_lost;
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}
