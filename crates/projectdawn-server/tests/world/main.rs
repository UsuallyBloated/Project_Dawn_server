//! The world's end-to-end tests: a real server in-process and real renet clients
//! through the whole handshake, one module per domain. The harness is `common`.
//! (Until 2026-11-01 this was one file, world_two_clients.rs.)

// Test code may unwrap and expect: a panic is the test failing.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// The readers decode a message, then match the one variant they want, as two
// steps; folding the match into the decode's pattern reads worse here.
#![allow(clippy::collapsible_match)]

mod common;

mod admin;
mod death;
mod durability;
mod groups_and_quests;
mod height;
mod hunger;
mod inventory;
mod melee;
mod pets;
mod presence;
mod smoke;
mod songs;
mod spells;
mod trade;
