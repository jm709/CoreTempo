//! The desktop shell's half of Sessions mode (spec 2026-08-27 §6): the shell
//! owns no session state, it proxies the sessions daemon's `/v1`.

pub mod client;
pub mod commands;
pub mod discovery;
pub mod pty;
pub mod sse;
pub mod supervisor;
