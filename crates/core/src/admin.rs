//! Admin actions on services and scheduled tasks, through a short-lived elevated helper.
//!
//! The app runs as the user. When it needs a service or task changed, [`client::Helper`]
//! launches `holdmyram-helper.exe` with one UAC prompt and talks to it over a private
//! named pipe that the app owns ([`pipe`]). The helper re-validates every request
//! against the hard-coded rules ([`rules`]) in its [`handler::Handler`], whatever the app
//! already checked, and only then calls Windows ([`win`]).
//!
//! - [`rules`]: never-touch services and task folders, name/path validators. ONE place.
//! - [`proto`]: the JSON-lines wire protocol (strict schema, size limits).
//! - [`handler`]: request handling + the [`handler::AdminBackend`] trait.
//! - [`win`]: the real backend (SCM + Task Scheduler COM). Helper only.
//! - [`fake`]: in-memory backend so every automated test runs without admin.
//! - [`pipe`]: pipe server (app), pipe client (helper), handshake, line framing.
//! - [`client`]: the app-side [`client::Helper`] handle and [`client::AdminApi`] calls.
//! - [`changelog`]: persistent changes made from the window, with undo.
//!
//! Limit (by design): code already running as the same user is not defended against;
//! UAC is not a security boundary against it.

pub mod changelog;
pub mod client;
pub mod fake;
pub mod handler;
pub mod pipe;
pub mod proto;
pub mod rules;
pub mod sys;
pub mod win;

pub use changelog::{AdminChange, ChangeLog};
pub use client::{AdminApi, Helper, LocalApi};
pub use handler::{AdminBackend, Handler, Policy};
pub use proto::{AdminError, ErrorCode, Op, Reply, ServiceState, StartChoice, StartType, TaskState};
