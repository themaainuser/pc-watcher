// pcw-agent library: the writer, the tracker loop and their helpers. The
// binary's tray wiring (Task 13) consumes these interfaces.

pub mod logfile;
pub mod state;
pub mod timers;
pub mod tracker;
pub mod writer;

pub use writer::{WriterConfig, WriterMsg, run_writer};
