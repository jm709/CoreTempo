//! The desktop shell's half of Sessions mode (spec 2026-08-27 §6): the shell
//! owns no session state, it proxies the sessions daemon's `/v1`.

pub mod client;
