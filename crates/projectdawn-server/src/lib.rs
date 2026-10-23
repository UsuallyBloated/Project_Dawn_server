//! Project Dawn server library — exposes the auth + db modules so
//! integration tests under `tests/` can drive them. The actual binary
//! entrypoint is in `main.rs`.

// No unsafe code in this crate, and none may be added without lifting this
// deliberately. The workspace's unsafe code lives elsewhere: the server
// binary's .env loader (main.rs) and gdext-net's extension entry point.
#![forbid(unsafe_code)]

pub mod auth;
pub mod char_data;
pub mod config;
pub mod db;
pub mod error;
pub mod world;

pub use config::Config;
pub use error::{AuthError, AuthResult};
