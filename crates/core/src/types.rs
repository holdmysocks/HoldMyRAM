//! Types shared across modules so they can be built independently.

use serde::{Deserialize, Serialize};

/// How a selected process is dealt with on toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// Ask windows to close, then force after the grace period.
    #[default]
    Close,
    /// Freeze the process; its memory stays allocated.
    Suspend,
    /// Terminate immediately.
    Kill,
}

/// One user-selected process, matched by executable name (case-insensitive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub name: String,
    #[serde(default)]
    pub method: Method,
}
