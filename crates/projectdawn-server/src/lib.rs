//! Project Dawn server library — exposes the auth + db modules so
//! integration tests under `tests/` can drive them. The actual binary
//! entrypoint is in `main.rs`.

pub mod auth;
pub mod config;
pub mod db;
pub mod error;

pub use config::Config;
pub use error::{AuthError, AuthResult};
