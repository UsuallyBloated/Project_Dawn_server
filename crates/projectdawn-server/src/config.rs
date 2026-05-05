//! Server config. Source of truth: env vars, with optional `config.toml`
//! fallback.

use anyhow::Result;
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub auth_bind: String,
    pub world_endpoint: String,
    pub database_url: String,
    pub min_client_version: semver::Version,
}

// We don't pull in the `semver` crate yet — implement the bare-minimum
// "is `client` >= `min`" comparison locally.
pub mod semver {
    use std::fmt;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Version {
        pub major: u32,
        pub minor: u32,
        pub patch: u32,
    }

    impl Version {
        pub fn parse(s: &str) -> Option<Self> {
            let mut parts = s.trim().split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            // Ignore pre-release / build metadata.
            let patch_field = parts.next().unwrap_or("0");
            let patch_str = patch_field
                .split(|c: char| c == '-' || c == '+')
                .next()
                .unwrap_or("0");
            let patch = patch_str.parse().ok()?;
            Some(Self { major, minor, patch })
        }

        pub fn at_least(&self, other: &Self) -> bool {
            (self.major, self.minor, self.patch)
                >= (other.major, other.minor, other.patch)
        }
    }

    impl fmt::Display for Version {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let auth_bind = env::var("PROJECTDAWN_AUTH_BIND")
            .unwrap_or_else(|_| "0.0.0.0:8765".into());
        let world_endpoint = env::var("PROJECTDAWN_WORLD_ENDPOINT")
            .unwrap_or_else(|_| "127.0.0.1:7777".into());
        let database_url = env::var("PROJECTDAWN_DATABASE_URL")
            .unwrap_or_else(|_| "sqlite://world.db?mode=rwc".into());
        let min_str = env::var("PROJECTDAWN_MIN_CLIENT_VERSION")
            .unwrap_or_else(|_| "0.1.0".into());
        let min_client_version = semver::Version::parse(&min_str)
            .ok_or_else(|| anyhow::anyhow!("invalid PROJECTDAWN_MIN_CLIENT_VERSION: {min_str}"))?;

        Ok(Self {
            auth_bind,
            world_endpoint,
            database_url,
            min_client_version,
        })
    }
}
