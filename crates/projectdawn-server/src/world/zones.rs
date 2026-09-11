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
    /// LEGACY / unused: per-kill XP is now computed from `level` via the EQ
    /// quadratic `progression::kill_xp` (mob_level^2 * ZEM), not this flat
    /// constant. Kept (defaulted) so existing `zone_camps.toml` entries still
    /// parse; safe to drop from the data in a later content pass.
    #[serde(default)]
    pub xp: i32,
    pub speed: f32,
    pub aggro: f32,
    /// Optional override; defaults to `aggro * 2.0` if missing.
    pub leash: Option<f32>,
    /// Optional override; defaults to `1.8` (matches `Enemy.melee_range`).
    pub melee_range: Option<f32>,
    /// Optional override; defaults to `2.5` (matches `Enemy.attack_interval`).
    pub attack_interval: Option<f32>,
    /// Named / boss mob id, looked up in `named_mobs.toml`. When set, the
    /// mob spawns with that entry's stat multipliers, enrage behaviour and
    /// guaranteed / rare drops applied on top of this template. An unknown or
    /// missing id simply behaves like an ordinary mob.
    #[serde(default)]
    pub named_id: Option<String>,
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
        // The phase 4 layout (2026-09-10): 21 camps (16 ordinary + 5 named
        // dens) with 54 spawn positions and 16 distinct mob names (each den
        // reuses its escort camp's template name, so a missing named_id
        // degrades to an ordinary mob). If this changes, change it
        // intentionally — the design doc is the client repo's
        // docs/design/phase4_content_plan.md.
        let camps = load_camps();
        assert_eq!(camps.len(), 54, "starter zone spawn-point count drifted");
        let names: std::collections::HashSet<&str> =
            camps.iter().map(|c| c.mob.name.as_str()).collect();
        assert_eq!(names.len(), 16, "starter zone mob-type count drifted");
    }

    #[test]
    fn every_named_mob_is_placed_exactly_once() {
        // The five authored named mobs each get one single-spawn den. A
        // named_id here that named_mobs.toml doesn't know would silently
        // spawn an ordinary mob, so pin the linkage from this side too.
        let camps = load_camps();
        let placed: Vec<&str> = camps
            .iter()
            .filter_map(|c| c.mob.named_id.as_deref())
            .collect();
        for id in ["sable", "rotfang", "ancient_crawler", "greth", "the_undying"] {
            assert_eq!(
                placed.iter().filter(|p| **p == id).count(),
                1,
                "named mob {id:?} should be placed exactly once"
            );
        }
        assert_eq!(placed.len(), 5, "unexpected extra named placements");
    }
}
