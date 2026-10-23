//! Wire-format types shared by client and server.
//!
//! Two namespaces:
//! - [`auth`] — JSON over WebSocket (launcher ↔ auth service).
//! - [`world`] — bincode over UDP/renet (game ↔ world simulation).
//!
//! These types are the contract. The Godot client mirrors them by hand
//! in `scripts/net/protocol.gd`; codegen comes later.

// No unsafe code in this crate, and none may be added without lifting this
// deliberately. The workspace's unsafe code lives elsewhere: the server
// binary's .env loader (main.rs) and gdext-net's extension entry point.
#![forbid(unsafe_code)]

pub mod auth;
pub mod world;
