//! Static zone-camp data. Source of truth lives in
//! `data/zone_camps.toml` (embedded at compile time via `include_str!`).
//!
//! Ported from `Project_Dawn/data/zone_data.gd` (`STARTER_ZONE_CAMPS`).
//! The game client used to instantiate `EnemySpawner` nodes from that
//! array at scene-ready; with server-authoritative enemies (Track 5),
//! the server owns all spawn-point state.
//!
//! Parse failure is a programmer error — `load_camps()` panics with a
//! useful message rather than returning a Result. The file is embedded
//! in the binary, so a parse failure can only happen if someone edits
//! the TOML to be malformed and the corresponding test below was
//! deleted or skipped.
//!
//! Coordinates are world-space (Vector3 in Godot — y is up). The
//! `spawns` arrays carry the canonical positions; `radius` adds an
//! XZ jitter when the spawn point fires.

use serde::Deserialize;

const ZONE_CAMPS_TOML: &str = include_str!("../../data/zone_camps.toml");

#[derive(Debug, Deserialize)]
struct ZoneCampsFile {
    default_radius: f32,
    default_respawn: f32,
    #[serde(rename = "camp")]
    camps: Vec<RawCamp>,
}

#[derive(Debug, Deserialize)]
struct RawCamp {
    #[allow(dead_code)] // descriptive only; not consumed by the runtime
    desc: String,
    mob: MobTemplate,
    spawns: Vec<[f32; 3]>,
    radius: Option<f32>,
    respawn: Option<f32>,
}

/// One mob archetype as authored in `zone_camps.toml`. Replicates the
/// `Enemy` node's authored exports for the fields the server simulates.
/// Fields beyond this set (resistances, caster spell damage, healer
/// flee threshold) are not represented in the starter camps and will
/// be added as later zones introduce them.
#[derive(Debug, Clone, Deserialize)]
pub struct MobTemplate {
    pub name: String,
    pub level: u32,
    pub hp: f32,
    pub dmg: i32,
    pub xp: i32,
    pub speed: f32,
    pub aggro: f32,
    /// Optional override; defaults to `aggro * 2.0` if missing.
    pub leash: Option<f32>,
    /// Optional override; defaults to `1.8` (matches `Enemy.melee_range`).
    pub melee_range: Option<f32>,
    /// Optional override; defaults to `2.5` (matches `Enemy.attack_interval`).
    pub attack_interval: Option<f32>,
}

/// One spawn-point definition. The `SpawnPoint` runtime type in
/// `spawn_points.rs` wraps this with mutable respawn-timer state.
#[derive(Debug, Clone)]
pub struct CampSpawn {
    pub pos: [f32; 3],
    pub radius: f32,
    pub respawn_secs: f32,
    pub mob: MobTemplate,
}

/// Parses the embedded TOML and flattens it into one `CampSpawn` per
/// authored spawn position. Order is preserved from the file so spawn
/// ids are deterministic across server restarts.
pub fn load_camps() -> Vec<CampSpawn> {
    let parsed: ZoneCampsFile = toml::from_str(ZONE_CAMPS_TOML)
        .expect("zone_camps.toml failed to parse — fix the embedded file");
    let mut out = Vec::new();
    for camp in parsed.camps {
        let radius = camp.radius.unwrap_or(parsed.default_radius);
        let respawn = camp.respawn.unwrap_or(parsed.default_respawn);
        for pos in camp.spawns {
            out.push(CampSpawn {
                pos,
                radius,
                respawn_secs: respawn,
                mob: camp.mob.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_toml_parses() {
        let camps = load_camps();
        assert!(
            !camps.is_empty(),
            "embedded zone_camps.toml produced zero spawn points"
        );
    }

    #[test]
    fn starter_camps_match_expected_shape() {
        // The starter zone has 9 camps with 27 spawn positions total.
        // If this changes, port the new shape from `zone_data.gd`
        // intentionally — silent drift means the server and client
        // disagree on what should be where.
        let camps = load_camps();
        assert_eq!(camps.len(), 27, "starter zone spawn-point count drifted");
        let names: std::collections::HashSet<&str> =
            camps.iter().map(|c| c.mob.name.as_str()).collect();
        assert_eq!(names.len(), 9, "starter zone mob-type count drifted");
    }
}
