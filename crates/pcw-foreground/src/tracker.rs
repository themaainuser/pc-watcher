//! Pure foreground-session state machine (spec §4.3). No `windows` imports:
//! the Win32 adapter (Task 10) feeds [`SessionSignal`]s into [`SessionTracker`].

use pcw_types::{AppSession, WindowTitle};
use serde::{Deserialize, Serialize};

/// Windows lock-screen process: never receives a session of its own.
pub const LOCK_APP: &str = "LockApp.exe";
/// A title must have been shown at least this long to be logged.
pub const TITLE_MIN_MS: u64 = 2_000;

/// Physical/logical event for the foreground tracker. Foreground/Title carry the
/// current foreground window; the paired events carry timestamped transitions.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionSignal {
    Foreground {
        ts_ms: u64,
        process_name: String,
        window_title: String,
    },
    Title {
        ts_ms: u64,
        process_name: String,
        window_title: String,
    },
    Locked {
        ts_ms: u64,
    },
    Unlocked {
        ts_ms: u64,
    },
    Suspended {
        ts_ms: u64,
    },
    Resumed {
        ts_ms: u64,
    },
    DisplayOff {
        ts_ms: u64,
    },
    DisplayOn {
        ts_ms: u64,
    },
    Paused {
        ts_ms: u64,
    },
    Unpaused {
        ts_ms: u64,
    },
}

/// What the tracker emits: a closed session, or a logged window title.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackerOutput {
    Session(AppSession),
    Title(WindowTitle),
}

/// The currently open session (not yet ended).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenSession {
    pub ts_start_ms: u64,
    pub process_name: String,
    pub window_title: String,
}

/// A title shown since `ts_ms`, logged when it is superseded or the session ends.
#[derive(Debug)]
struct PendingTitle {
    ts_ms: u64,
    process_name: String,
    title: String,
}

/// Tracks foreground sessions and window-title dwell times.
///
/// Invariants (active = none of locked/suspended/display_off/paused):
/// - exactly one open session while active and the current process is not `LockApp.exe`;
/// - `pending` is `Some` only while a session is open.
#[derive(Debug)]
pub struct SessionTracker {
    capture_titles: bool,
    /// Current foreground `(process, title)`, tracked even while inactive.
    current: Option<(String, String)>,
    open: Option<OpenSession>,
    pending: Option<PendingTitle>,
    /// Last `(process, title)` pair that was emitted; consecutive identical
    /// titles are logged once per process, so this is kept globally across
    /// sessions (a different app showing the same title is still logged).
    last_emitted: Option<(String, String)>,
    locked: bool,
    suspended: bool,
    display_off: bool,
    paused: bool,
}

impl SessionTracker {
    pub fn new(capture_titles: bool) -> Self {
        Self {
            capture_titles,
            current: None,
            open: None,
            pending: None,
            last_emitted: None,
            locked: false,
            suspended: false,
            display_off: false,
            paused: false,
        }
    }

    /// Snapshots the currently open session, if any.
    pub fn open_session(&self) -> Option<OpenSession> {
        self.open.clone()
    }

    fn active(&self) -> bool {
        !(self.locked || self.suspended || self.display_off || self.paused)
    }

    pub fn on_signal(&mut self, s: SessionSignal) -> Vec<TrackerOutput> {
        match s {
            SessionSignal::Foreground {
                ts_ms,
                process_name,
                window_title,
            } => self.on_foreground(ts_ms, process_name, window_title),
            SessionSignal::Title {
                ts_ms,
                process_name,
                window_title,
            } => self.on_title(ts_ms, process_name, window_title),
            SessionSignal::Locked { ts_ms } => self.set_flag(Flag::Locked, ts_ms),
            SessionSignal::Unlocked { ts_ms } => self.clear_flag(Flag::Locked, ts_ms),
            SessionSignal::Suspended { ts_ms } => self.set_flag(Flag::Suspended, ts_ms),
            SessionSignal::Resumed { ts_ms } => self.clear_flag(Flag::Suspended, ts_ms),
            SessionSignal::DisplayOff { ts_ms } => self.set_flag(Flag::DisplayOff, ts_ms),
            SessionSignal::DisplayOn { ts_ms } => self.clear_flag(Flag::DisplayOff, ts_ms),
            SessionSignal::Paused { ts_ms } => self.set_flag(Flag::Paused, ts_ms),
            SessionSignal::Unpaused { ts_ms } => self.clear_flag(Flag::Paused, ts_ms),
        }
    }

    /// Ends the (single) open session, applying the title rule at `ts_ms`.
    pub fn finalize(&mut self, ts_ms: u64) -> Vec<TrackerOutput> {
        self.close_open(ts_ms)
    }

    fn on_foreground(&mut self, ts: u64, process: String, title: String) -> Vec<TrackerOutput> {
        let mut out = Vec::new();
        let title = self.apply_capture_flag(title);
        let same_process = self.current.as_ref().is_some_and(|(p, _)| *p == process);
        self.current = Some((process.clone(), title.clone()));

        if !self.active() {
            return out;
        }

        if process == LOCK_APP {
            // LockApp never gets a session; the previous foreground's ends at ts.
            return self.close_open(ts);
        }

        if same_process {
            // Same process: not a session boundary, just a title change.
            // The open session keeps its opening title (spec §4.3); the
            // change is only offered to the title log.
            if let Some(t) = self.emit_pending_if_due(ts) {
                out.push(t);
            }
            self.offer_title(ts, process.clone(), title.clone());
        } else {
            out.extend(self.close_open(ts));
        }
        if self.open.is_none() {
            // Either a different process (its session opens at ts), or the
            // current foreground still lacks a session because one was
            // already finalized (e.g. shutdown tracking): reopen it.
            self.open = Some(OpenSession {
                ts_start_ms: ts,
                process_name: process.clone(),
                window_title: title.clone(),
            });
            // The opening title becomes the pending title.
            self.offer_title(ts, process, title);
        }
        out
    }

    fn on_title(&mut self, ts: u64, process: String, title: String) -> Vec<TrackerOutput> {
        if self.current.as_ref().is_some_and(|(p, _)| *p != process) {
            // A title for a different process than the tracked foreground is
            // unexpected input; ignore it rather than corrupt state.
            return Vec::new();
        }
        let title = self.apply_capture_flag(title);
        if let Some((_, t)) = self.current.as_mut() {
            *t = title.clone();
        }
        if !self.active() {
            return Vec::new();
        }
        if self.open.is_none() {
            return Vec::new();
        }
        // The open session keeps the title it opened with (spec §4.3); this
        // event only feeds the title log.
        let mut out = Vec::new();
        if let Some(t) = self.emit_pending_if_due(ts) {
            out.push(t);
        }
        self.offer_title(ts, process, title);
        out
    }

    /// Titles are captured only when `capture_titles` is set; otherwise the
    /// tracker stores empty titles and logs none (spec §4.3).
    fn apply_capture_flag(&self, title: String) -> String {
        if self.capture_titles {
            title
        } else {
            String::new()
        }
    }

    /// Emits the pending title if it was shown at least `TITLE_MIN_MS` and
    /// differs from the last emitted `(process, title)` pair. When due, the
    /// pending entry is consumed (`last_emitted` updated); when not due it
    /// stays anchored so a later event can still flush it from its original
    /// timestamp.
    fn emit_pending_if_due(&mut self, at_ts: u64) -> Option<TrackerOutput> {
        let p = self.pending.as_ref()?;
        if at_ts.saturating_sub(p.ts_ms) < TITLE_MIN_MS {
            return None;
        }
        if self
            .last_emitted
            .as_ref()
            .is_some_and(|(proc, title)| *proc == p.process_name && *title == p.title)
        {
            self.pending = None;
            return None;
        }
        self.last_emitted = Some((p.process_name.clone(), p.title.clone()));
        let out = TrackerOutput::Title(WindowTitle {
            ts_ms: p.ts_ms,
            process_name: p.process_name.clone(),
            title: p.title.clone(),
        });
        self.pending = None;
        Some(out)
    }

    /// Offers a new title as the pending entry. An identical title does not
    /// replace the pending one (it keeps its earlier anchor for dwell timing).
    fn offer_title(&mut self, ts: u64, process: String, title: String) {
        if !self.capture_titles || title.is_empty() {
            return;
        }
        if self.pending.as_ref().is_some_and(|p| p.title == title) {
            return;
        }
        self.pending = Some(PendingTitle {
            ts_ms: ts,
            process_name: process,
            title,
        });
    }

    /// Ends the open session at `ts`, flushing the title rule at that instant.
    fn close_open(&mut self, ts: u64) -> Vec<TrackerOutput> {
        let open = match self.open.take() {
            Some(open) => open,
            None => return Vec::new(),
        };
        let flushed = self.emit_pending_if_due(ts);
        self.pending = None; // the pending entry belongs to this session only
        let mut out = vec![TrackerOutput::Session(AppSession {
            ts_start_ms: open.ts_start_ms,
            // An out-of-order end timestamp must not precede the start one.
            ts_end_ms: ts.max(open.ts_start_ms),
            process_name: open.process_name,
            window_title: open.window_title,
        })];
        if let Some(t) = flushed {
            out.push(t);
        }
        out
    }

    fn set_flag(&mut self, flag: Flag, ts: u64) -> Vec<TrackerOutput> {
        if flag.is_set(self) {
            return Vec::new();
        }
        match flag {
            Flag::Locked => self.locked = true,
            Flag::Suspended => self.suspended = true,
            Flag::DisplayOff => self.display_off = true,
            Flag::Paused => self.paused = true,
        }
        // Going inactive: the open session ends at ts and nothing reopens.
        self.close_open(ts)
    }

    /// Flags are a set, not a counter: a paired clear only reopens the session
    /// when it was the last one set (i.e. we actually become active again).
    fn clear_flag(&mut self, flag: Flag, ts: u64) -> Vec<TrackerOutput> {
        if !flag.is_set(self) {
            return Vec::new();
        }
        match flag {
            Flag::Locked => self.locked = false,
            Flag::Suspended => self.suspended = false,
            Flag::DisplayOff => self.display_off = false,
            Flag::Paused => self.paused = false,
        }
        if !self.active() {
            return Vec::new();
        }
        self.reopen(ts)
    }

    /// Opens a session for the current foreground, unless it is unknown or
    /// LockApp. The opening title becomes the pending title.
    fn reopen(&mut self, ts: u64) -> Vec<TrackerOutput> {
        let (process, title) = match self.current.as_ref() {
            Some((process, title)) => (process.clone(), title.clone()),
            None => return Vec::new(),
        };
        if process == LOCK_APP {
            return Vec::new();
        }
        self.open = Some(OpenSession {
            ts_start_ms: ts,
            process_name: process.clone(),
            window_title: title.clone(),
        });
        self.offer_title(ts, process, title);
        Vec::new()
    }
}

#[derive(Debug, Clone, Copy)]
enum Flag {
    Locked,
    Suspended,
    DisplayOff,
    Paused,
}

impl Flag {
    fn is_set(self, t: &SessionTracker) -> bool {
        match self {
            Self::Locked => t.locked,
            Self::Suspended => t.suspended,
            Self::DisplayOff => t.display_off,
            Self::Paused => t.paused,
        }
    }
}
