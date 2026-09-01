//! The desktop shell's Rust half. Everything but `fn main` and the invoke-handler
//! factory lives here so integration tests can link it; `main.rs` is the Tauri
//! entry point over this library.

pub mod bridge;
pub mod commands;
pub mod merge;
pub mod sessions;
pub mod state;
