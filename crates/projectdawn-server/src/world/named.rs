//! Named / boss mobs — server-side.
//!
//! Mirror of the client's `data/named_mob_definitions.gd`, in the same spirit
//! as `items.rs` mirroring the `.tres` files. Before this existed the server
//! had no concept of a named mob at all: the client pre-multiplied the stats
//! locally and sent a `DevSpawnMob` carrying six plain numbers, so online every
//! named mob was a generic mob wearing a fancy display name — no enrage, and
//! none of its guaranteed or rare drops.
//!
//! Three things live here:
//!   * **stat multipliers** applied at spawn (`hp_mult`, `damage_mult`,
//!     `xp_mult`, plus a `level` override),
//!   * **enrage** config, fired once per life when HP crosses the threshold,
//!   * **guaranteed / rare loot**, resolved at death.
//!
//! The multipliers scale whatever `MobTemplate` the mob spawned from, so
//! tagging a camp entry with `named_id` makes that camp's mob a bigger version
//! of itself. A dev spawn by id has no base template and uses the client's
//! enemy-scene defaults instead, so the online result matches the offline one.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

const NAMED_TOML: &str = include_str!("../../data/named_mobs.toml");

/// Base stats of the client's `enemy.tscn`, used when a named mob is spawned
/// by id with no underlying camp template (the Test Panel path). Keeping these
/// here rather than at the call site means the online spawn produces the same
/// creature the offline one does.
pub const SCENE_BASE_HP: f32 = 50.0;
pub const SCENE_BASE_DMG: i32 = 5;
pub const SCENE_BASE_SPEED: f32 = 2.5;
pub const SCENE_BASE_AGGRO: f32 = 10.0;

#[derive(Debug, Clone, Deserialize)]
pub struct RareDrop {
    pub path: String,
    pub drop_chance: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NamedMob {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default = "one_f32")]
    pub hp_mult: f32,
    #[serde(default = "one_f32")]
    pub damage_mult: f32,
    #[serde(default = "one_f32")]
    pub xp_mult: f32,
    pub level: u32,
    /// Fraction of max HP at which enrage fires. `0.0` disables it.
    #[serde(default)]
    pub enrage_threshold: f32,
    #[serde(default = "one_f32")]
    pub enrage_damage_mult: f32,
    #[serde(default = "one_f32")]
    pub enrage_speed_mult: f32,
    #[serde(default)]
    pub guaranteed_loot: Vec<String>,
    #[serde(default)]
    pub rare_loot: Vec<RareDrop>,
}

impl NamedMob {
    /// Nameplate text: `"Rotfang the Feared"`, or just the name when the
    /// subtitle is empty.
    pub fn full_name(&self) -> String {
        if self.subtitle.is_empty() {
            self.display_name.clone()
        } else {
            format!("{} {}", self.display_name, self.subtitle)
        }
    }

    pub fn enrages(&self) -> bool {
        self.enrage_threshold > 0.0
    }
}

fn one_f32() -> f32 {
    1.0
}

#[derive(Debug, Deserialize)]
struct NamedFile {
    named: Vec<NamedMob>,
}

fn table() -> &'static HashMap<String, NamedMob> {
    static NAMED: OnceLock<HashMap<String, NamedMob>> = OnceLock::new();
    NAMED.get_or_init(|| {
        let parsed: NamedFile = toml::from_str(NAMED_TOML)
            .expect("named_mobs.toml must parse — fix the embedded file");
        parsed
            .named
            .into_iter()
            .map(|m| (m.id.clone(), m))
            .collect()
    })
}

/// Look up a named mob by id. `None` for unknown ids, so an untagged or
/// mistyped mob simply behaves like an ordinary one rather than failing.
pub fn lookup(id: &str) -> Option<&'static NamedMob> {
    if id.is_empty() {
        return None;
    }
    table().get(id)
}

/// Look up a named mob by the nameplate text it spawns with.
///
/// `Entity::apply_named` sets `mob.name` to exactly `full_name()`, so this is
/// an exact reverse of that, not a fuzzy match. It exists because the three
/// kill paths that roll loot have only the mob's name in scope, not its id,
/// and threading an id through all of them would touch far more code than a
/// deterministic lookup does.
pub fn lookup_by_display_name(full_name: &str) -> Option<&'static NamedMob> {
    if full_name.is_empty() {
        return None;
    }
    table().values().find(|m| m.full_name() == full_name)
}

/// Resolve a dev-spawn request to a named mob by the name the client sent.
///
/// The Test Panel sends `display_name` ("Rotfang"), while a spawned mob's
/// nameplate is `full_name()` ("Rotfang the Feared"), so both spellings are
/// accepted. This is what lets the existing Test Panel spawn a real named mob
/// with no wire change: the id is recovered server-side from a name the client
/// already sends.
pub fn resolve_for_dev_spawn(name: &str) -> Option<&'static NamedMob> {
    if name.is_empty() {
        return None;
    }
    table()
        .values()
        .find(|m| m.display_name == name || m.full_name() == name)
}

/// Every named id, for dev tooling and tests.
pub fn all_ids() -> Vec<&'static str> {
    table().keys().map(|s| s.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_toml_parses() {
        assert!(!table().is_empty(), "named_mobs.toml produced no entries");
    }

    #[test]
    fn rotfang_matches_the_client_definition() {
        let m = lookup("rotfang").expect("rotfang exists");
        assert_eq!(m.full_name(), "Rotfang the Feared");
        assert_eq!(m.hp_mult, 3.5);
        assert_eq!(m.damage_mult, 1.8);
        assert_eq!(m.xp_mult, 4.0);
        assert_eq!(m.level, 6);
        assert_eq!(m.enrage_threshold, 0.20);
        assert_eq!(m.guaranteed_loot.len(), 1);
        assert_eq!(m.rare_loot.len(), 1);
        assert_eq!(m.rare_loot[0].drop_chance, 0.30);
    }

    /// Sable is the one named mob with enrage switched off, and the client's
    /// header documents `0` as the disable value. If a future edit gives it a
    /// threshold by accident, this catches it.
    #[test]
    fn sable_does_not_enrage_and_the_others_do() {
        assert!(!lookup("sable").expect("sable").enrages());
        for id in ["rotfang", "greth", "ancient_crawler", "the_undying"] {
            assert!(lookup(id).expect(id).enrages(), "{id} should enrage");
        }
    }

    /// The Test Panel sends `display_name`, not the nameplate text, so a dev
    /// spawn must resolve from either spelling. This is what lets the existing
    /// panel spawn a real named mob with no wire change.
    #[test]
    fn dev_spawn_resolves_from_either_spelling() {
        let by_display = resolve_for_dev_spawn("Rotfang").expect("display name");
        let by_full = resolve_for_dev_spawn("Rotfang the Feared").expect("full name");
        assert_eq!(by_display.id, "rotfang");
        assert_eq!(by_full.id, "rotfang");

        // A mob with no subtitle has both spellings identical.
        assert_eq!(
            resolve_for_dev_spawn("Ancient Crawler").expect("crawler").id,
            "ancient_crawler"
        );

        // An ordinary dev spawn is untouched.
        assert!(resolve_for_dev_spawn("Plague Rat").is_none());
        assert!(resolve_for_dev_spawn("").is_none());
    }

    #[test]
    fn unknown_and_empty_ids_are_not_named() {
        assert!(lookup("").is_none());
        assert!(lookup("no_such_mob").is_none());
    }

    /// Every drop path must be a real client resource path. A typo here would
    /// surface as an item that silently never drops.
    #[test]
    fn every_loot_path_looks_like_a_resource_path() {
        for id in all_ids() {
            let m = lookup(id).unwrap();
            for p in &m.guaranteed_loot {
                assert!(p.starts_with("res://data/loot/items/"), "{id}: {p}");
            }
            for r in &m.rare_loot {
                assert!(r.path.starts_with("res://data/loot/items/"), "{id}: {}", r.path);
                assert!(
                    r.drop_chance > 0.0 && r.drop_chance <= 1.0,
                    "{id}: drop_chance out of range"
                );
            }
        }
    }
}
