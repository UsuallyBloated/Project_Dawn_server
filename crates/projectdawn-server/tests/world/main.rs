//! The world's end-to-end tests: a real server in-process and real renet clients
//! through the whole handshake, one module per domain. The harness is `common`.
//! (Until 2026-11-01 this was one file, world_two_clients.rs.)

mod common;

mod trade;
mod hunger;
mod presence;
mod pets;
mod admin;
mod durability;
mod songs;
mod spells;
mod melee;
mod inventory;
mod death;
mod groups_and_quests;
mod smoke;
