//! Gameplay constants and pure formulas shared by the server and (eventually)
//! a thin Godot-side mirror via GDExtension.
//!
//! Empty for now — populated as combat math moves server-side.

// No unsafe code in this crate, and none may be added without lifting this
// deliberately. The workspace's unsafe code lives elsewhere: the server
// binary's .env loader (main.rs) and gdext-net's extension entry point.
#![forbid(unsafe_code)]
