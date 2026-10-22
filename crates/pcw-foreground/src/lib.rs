// pcw-foreground: foreground window tracking.
//
// Pure session state machine lives here; the Win32 adapter (Task 10) feeds
// `SessionSignal`s into `SessionTracker` and consumes `TrackerOutput`s.

mod tracker;

pub use tracker::{
    LOCK_APP, OpenSession, SessionSignal, SessionTracker, TITLE_MIN_MS, TrackerOutput,
};
