//! HoldMyRAM engine. No UI dependencies, so it can be tested on its own.

pub mod actions;
pub mod admin;
pub mod config;
pub mod journal;
mod plan;
mod process;
pub mod risk;
pub mod scan;
pub mod startup;
pub mod toggle;
pub mod types;

pub use plan::{ancestors, descendants, select_targets, Skipped};
pub use process::{foreground_pid, list_processes, ProcInfo};
pub use types::{Method, Selection};
