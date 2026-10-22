// pcw-foreground: foreground window tracking.
//
// Pure session state machine lives here; the Win32 adapter (Task 10) feeds
// `SessionSignal`s into `SessionTracker` and consumes `TrackerOutput`s.

pub mod os;
pub mod sys;
mod tracker;

pub use os::{HookHandle, OsError, start_hooks};
pub use sys::{allow_taskbar_created_message, attach_parent_console, free_disk_bytes, is_elevated};
pub use tracker::{
    LOCK_APP, OpenSession, SessionSignal, SessionTracker, TITLE_MIN_MS, TrackerOutput,
};
