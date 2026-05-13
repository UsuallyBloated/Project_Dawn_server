//! Track 6 sub-task 2 — server-side weapon item table.
//!
//! Mirror of the client's per-item .tres files. The damage formula port
//! reads `damage_min` / `damage_max` / `is_ranged` / `skill` from here
//! based on the `weapon_path` the client sends with each `Attack` intent.
//!
//! Unknown paths fall back to bare-handed damage (1-4 + STR bonus).
//! Inventory authority is a future track; until then, the client picks
//! which weapon path to send — meaning a determined cheater can claim
//! they're wielding the strongest weapon in the table. Acceptable for
//! Track 6's scope per handoff Q3.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

const ITEMS_TOML: &str = include_str!("../../data/items.toml");

#[derive(Debug, Deserialize)]
struct ItemsFile {
    #[serde(rename = "weapon")]
    weapons: Vec<Weapon>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // weapon_delay + skill land with sub-task 2b (server-paced auto-attack + skill-multiplier port)
pub struct Weapon {
    pub path: String,
    pub name: String,
    pub damage_min: i32,
    pub damage_max: i32,
    pub weapon_delay: f32,
    pub skill: String,
    #[serde(default)]
    pub is_ranged: bool,
}

fn weapons() -> &'static HashMap<String, Weapon> {
    static WEAPONS: OnceLock<HashMap<String, Weapon>> = OnceLock::new();
    WEAPONS.get_or_init(|| {
        let parsed: ItemsFile = toml::from_str(ITEMS_TOML)
            .expect("items.toml must parse — fix the embedded file");
        parsed
            .weapons
            .into_iter()
            .map(|w| (w.path.clone(), w))
            .collect()
    })
}

/// Look up a weapon by its client-side resource path. Returns `None` for
/// empty paths (bare-handed swing) or paths not present in the table
/// (test-panel-generated items without a .tres file, or future weapons
/// that haven't been mirrored yet — both fall back to fists in
/// `combat::calc_damage`).
pub fn lookup(path: &str) -> Option<&'static Weapon> {
    if path.is_empty() {
        return None;
    }
    weapons().get(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_toml_parses() {
        // Force-init the OnceLock; panic-on-parse means a failure here
        // surfaces as a clean test failure rather than a runtime crash.
        let table = weapons();
        assert!(!table.is_empty(), "items.toml produced no weapons");
    }

    #[test]
    fn iron_short_sword_resolves() {
        let w = lookup("res://data/loot/items/iron_short_sword.tres")
            .expect("iron short sword in table");
        assert_eq!(w.damage_min, 7);
        assert_eq!(w.damage_max, 15);
        assert_eq!(w.skill, "1h_slashing");
        assert!(!w.is_ranged);
    }

    #[test]
    fn empty_path_returns_none() {
        assert!(lookup("").is_none());
    }

    #[test]
    fn unknown_path_returns_none() {
        assert!(lookup("res://data/loot/items/golden_sword_of_lies.tres").is_none());
    }
}
