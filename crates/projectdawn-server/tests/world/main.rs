//! The world's end-to-end tests: a real server in-process and real renet clients
//! through the whole handshake, one module per domain. The harness is `common`.
//! (Until 2026-11-01 this was one file, world_two_clients.rs.)

mod common;

mod admin;
mod death;
mod durability;
mod groups_and_quests;
mod hunger;
mod inventory;
mod melee;
mod pets;
mod presence;
mod smoke;
mod songs;
mod spells;
mod trade;
