//! `CoreTempo` core library: PTY manager, agent-state tracking, message routing,
//! event bus, `SQLite` store, and HTTP API. Zero UI dependencies.
//!
//! With `default-features = false` only the wire types (`types`, `time`) compile.

pub mod export;
pub mod time;
pub mod types;

#[cfg(feature = "server")]
pub mod api;

#[cfg(feature = "server")]
pub mod bus;

#[cfg(feature = "server")]
pub mod pty;

#[cfg(feature = "server")]
pub mod router;

#[cfg(feature = "server")]
pub mod run;

#[cfg(feature = "server")]
pub mod schema;

#[cfg(feature = "server")]
pub mod store;

#[cfg(feature = "server")]
pub mod trigger;

#[cfg(feature = "server")]
pub mod workflow;
