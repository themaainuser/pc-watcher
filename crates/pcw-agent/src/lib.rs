// pcw-agent library: the writer thread and its helpers. The binary's
// install/tray wiring (Task 13) consumes these interfaces.

pub mod logfile;
pub mod state;
pub mod timers;
pub mod writer;

pub use writer::{WriterConfig, WriterMsg, run_writer};
