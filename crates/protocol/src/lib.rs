//! Wire-format types shared by client and server.
//!
//! Two namespaces:
//! - [`auth`] — JSON over WebSocket (launcher ↔ auth service).
//! - [`world`] — bincode over UDP/renet (game ↔ world simulation).
//!
//! These types are the contract. The Godot client mirrors them by hand
//! in `scripts/net/protocol.gd`; codegen comes later.

pub mod auth;
pub mod world;
