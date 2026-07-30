//! Track 14.1 — server-side item registry.
//!
//! Mirror of the client's per-item `.tres` files. Generated from
//! `Project_Dawn/data/loot/items/*.tres` via
//! `Project_Dawn/tools/export_items_oneshot.py` (or the canonical
//! editor script `tools/export_items.gd`) into `data/items.toml`.
//!
//! Replaces the Track 6 `Weapon`-only table. The struct keeps the
//! same field names as the old `Weapon` (`damage_min`, `damage_max`,
//! `weapon_delay`, `skill`, `is_ranged`) so the combat / tick call
//! sites that read those fields don't change. The new fields drive
//! Track 14 work: `is_equippable_in_slot` validates equip intents
//! against the paperdoll slot, `max_stack` caps stack growth on
//! loot, and `bag_num_slots` is consumed by Track 14.3's bag wiring.
//!
//! Unknown paths still return `None`; callers fall back as before
//! (bare-handed damage, melee range, unlimited stack).

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

const ITEMS_TOML: &str = include_str!("../../data/items.toml");

/// Mirrors `ItemData.Type` in `scripts/item_data.gd`. Numeric order
/// matches the client enum so the `type = N` integer in the .tres
/// file maps cleanly when the exporter writes the snake_case
/// discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemType {
    Weapon,
    Offhand,
    Head,
    Chest,
    Legs,
    Feet,
    Hands,
    Ring,
    Neck,
    Consumable,
    Misc,
    Augment,
    Bag,
}

fn default_stack_size() -> u32 {
    1
}

fn default_weapon_delay() -> f32 {
    2.0
}

#[derive(Debug, Deserialize)]
struct ItemsFile {
    #[serde(rename = "item")]
    items: Vec<Item>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // many fields land with Track 14.2 (stat recompute) and the vendor follow-up
pub struct Item {
    pub path: String,
    pub name: String,
    pub item_type: ItemType,
    #[serde(default = "default_stack_size")]
    pub stack_size: u32,
    #[serde(default)]
    pub rarity: u32,

    // Weapon
    #[serde(default)]
    pub damage_min: i32,
    #[serde(default)]
    pub damage_max: i32,
    #[serde(default = "default_weapon_delay")]
    pub weapon_delay: f32,
    #[serde(default)]
    pub skill: String,
    #[serde(default)]
    pub is_two_handed: bool,
    #[serde(default)]
    pub is_ranged: bool,

    // Armor
    #[serde(default)]
    pub armor: i32,
    #[serde(default)]
    pub armor_type: String,

    // Stat affixes (Track 14.2 reads these on equip / unequip).
    #[serde(default)]
    pub str_bonus: i32,
    #[serde(default)]
    pub dex_bonus: i32,
    #[serde(default)]
    pub agi_bonus: i32,
    #[serde(default)]
    pub int_bonus: i32,
    #[serde(default)]
    pub wis_bonus: i32,
    #[serde(default)]
    pub cha_bonus: i32,
    #[serde(default)]
    pub con_bonus: i32,
    #[serde(default)]
    pub max_hp_bonus: f32,
    #[serde(default)]
    pub max_mp_bonus: f32,
    #[serde(default)]
    pub max_stamina_bonus: f32,

    // Bag
    #[serde(default)]
    pub bag_num_slots: u32,

    // Consumable
    #[serde(default)]
    pub heal_on_use: f32,
    #[serde(default)]
    pub mp_on_use: f32,
    #[serde(default)]
    pub is_food: bool,
    #[serde(default)]
    pub is_drink: bool,
    #[serde(default)]
    pub food_hp_regen: f32,
    #[serde(default)]
    pub food_mp_regen: f32,
    #[serde(default)]
    pub food_duration: f32,

    // Proc weapons
    #[serde(default)]
    pub proc_chance: f32,
    #[serde(default)]
    pub proc_damage: i32,
    #[serde(default)]
    pub proc_damage_type: u8,
    #[serde(default)]
    pub proc_name: String,

    // Augmentation
    #[serde(default)]
    pub gem_slots: u32,

    // Vendor
    #[serde(default)]
    pub vendor_price: u32,
}

fn items() -> &'static HashMap<String, Item> {
    static ITEMS: OnceLock<HashMap<String, Item>> = OnceLock::new();
    ITEMS.get_or_init(|| {
        let parsed: ItemsFile = toml::from_str(ITEMS_TOML)
            .expect("items.toml must parse — re-run export_items_oneshot.py if it's stale");
        parsed
            .items
            .into_iter()
            .map(|i| (i.path.clone(), i))
            .collect()
    })
}

/// Look up an item by its client-side resource path. Returns `None`
/// for empty paths or paths not in the registry (runtime-built items
/// without a `.tres`, or future items that haven't been re-exported
/// yet — combat falls back to bare-handed damage, inventory falls
/// back to unlimited stack).
pub fn lookup(path: &str) -> Option<&'static Item> {
    if path.is_empty() {
        return None;
    }
    items().get(path)
}

/// Track 14 follow-up — look up an item by its human-readable name
/// (matches `ItemData.item_name`). Used by the vendor BuyItem
/// dispatch, which receives the item by display name. O(N) linear
/// scan; 158 items today and the call site is one-shot per buy
/// click, so we don't bother memoising a name → path index.
pub fn lookup_by_name(name: &str) -> Option<&'static Item> {
    if name.is_empty() {
        return None;
    }
    items().values().find(|i| i.name == name)
}

/// Track 14.1 — equip-slot validation. Maps an item to whether it
/// can sit in the given paperdoll slot. The slot indices match
/// `protocol::world::EquipSlot` order (weapon=0, offhand=1, head=2,
/// chest=3, legs=4, feet=5, hands=6, ring=7, neck=8). Unknown paths
/// reject — the server only trusts items it has authored data for.
///
/// Weapons may sit in slot 0 (main hand) or slot 1 (offhand) — the
/// dual-wield skill gate stays client-side for now (the server-auth
/// skill leveling pass is a later track).
pub fn is_equippable_in_slot(path: &str, slot: u8) -> bool {
    let Some(item) = lookup(path) else {
        return false;
    };
    match (item.item_type, slot) {
        (ItemType::Weapon, 0) | (ItemType::Weapon, 1) => true,
        (ItemType::Offhand, 1) => true,
        (ItemType::Head, 2) => true,
        (ItemType::Chest, 3) => true,
        (ItemType::Legs, 4) => true,
        (ItemType::Feet, 5) => true,
        (ItemType::Hands, 6) => true,
        (ItemType::Ring, 7) => true,
        (ItemType::Neck, 8) => true,
        _ => false,
    }
}

/// Track 14.1 — max stack size for a path. Returns `u32::MAX` for
/// unknown paths so legacy stacking (unlimited) is preserved until
/// every item is in the registry.
pub fn max_stack(path: &str) -> u32 {
    lookup(path).map(|i| i.stack_size).unwrap_or(u32::MAX)
}

/// Track 14.3 — bag inner slot count. `Some(n)` for BAG-typed items;
/// `None` for non-bag or unknown paths.
#[allow(dead_code)] // wired in Track 14.3's bag-locations work
pub fn bag_num_slots(path: &str) -> Option<u32> {
    let item = lookup(path)?;
    if matches!(item.item_type, ItemType::Bag) && item.bag_num_slots > 0 {
        Some(item.bag_num_slots)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_toml_parses() {
        // Force-init the OnceLock; panic-on-parse means a failure here
        // surfaces as a clean test failure rather than a runtime crash.
        let table = items();
        assert!(!table.is_empty(), "items.toml produced no items");
    }

    #[test]
    fn iron_short_sword_resolves_as_weapon() {
        let w = lookup("res://data/loot/items/iron_short_sword.tres")
            .expect("iron short sword in table");
        assert_eq!(w.damage_min, 7);
        assert_eq!(w.damage_max, 15);
        assert_eq!(w.skill, "1h_slashing");
        assert!(!w.is_ranged);
        assert_eq!(w.item_type, ItemType::Weapon);
    }

    #[test]
    fn flamebrand_proc_data_resolves_as_fire() {
        // Server-authoritative procs read these off the equipped weapon. The
        // element is authored in SpellData space (FIRE=0); it was mis-tagged 1
        // (ICE) — a fire sword must be 0. Guard it so it can't regress.
        let w = lookup("res://data/loot/items/flamebrand.tres").expect("flamebrand in table");
        assert!((w.proc_chance - 0.15).abs() < 0.001);
        assert_eq!(w.proc_damage, 25);
        assert_eq!(w.proc_damage_type, 0, "Flamebrand procs FIRE (SpellData 0), not ICE");
        assert_eq!(w.proc_name, "Flaming Strike");
    }

    #[test]
    fn cloth_robe_resolves_as_chest() {
        let i = lookup("res://data/loot/items/cloth_robe.tres")
            .expect("cloth robe in table");
        assert_eq!(i.item_type, ItemType::Chest);
        assert_eq!(i.armor, 4);
        assert_eq!(i.armor_type, "cloth");
    }

    #[test]
    fn minor_healing_potion_resolves_as_consumable_with_stack() {
        let i = lookup("res://data/loot/items/minor_healing_potion.tres")
            .expect("potion in table");
        assert_eq!(i.item_type, ItemType::Consumable);
        assert_eq!(i.stack_size, 10);
        assert_eq!(i.heal_on_use, 50.0);
    }

    #[test]
    fn empty_path_returns_none() {
        assert!(lookup("").is_none());
    }

    #[test]
    fn unknown_path_returns_none() {
        assert!(lookup("res://data/loot/items/golden_sword_of_lies.tres").is_none());
    }

    #[test]
    fn is_equippable_weapon_in_weapon_or_offhand() {
        let p = "res://data/loot/items/iron_short_sword.tres";
        assert!(is_equippable_in_slot(p, 0));
        assert!(is_equippable_in_slot(p, 1));
        assert!(!is_equippable_in_slot(p, 2)); // head
        assert!(!is_equippable_in_slot(p, 3)); // chest
    }

    #[test]
    fn is_equippable_chest_only_in_chest_slot() {
        let p = "res://data/loot/items/cloth_robe.tres";
        assert!(is_equippable_in_slot(p, 3));
        assert!(!is_equippable_in_slot(p, 0));
        assert!(!is_equippable_in_slot(p, 1));
    }

    #[test]
    fn is_equippable_consumable_never() {
        let p = "res://data/loot/items/minor_healing_potion.tres";
        for slot in 0..=8u8 {
            assert!(
                !is_equippable_in_slot(p, slot),
                "potion must not be equippable in slot {slot}"
            );
        }
    }

    #[test]
    fn is_equippable_unknown_path_never() {
        assert!(!is_equippable_in_slot("res://nope.tres", 0));
        assert!(!is_equippable_in_slot("", 0));
    }

    #[test]
    fn max_stack_known_vs_unknown() {
        assert_eq!(
            max_stack("res://data/loot/items/minor_healing_potion.tres"),
            10
        );
        assert_eq!(
            max_stack("res://data/loot/items/iron_short_sword.tres"),
            1
        );
        // Unknown path falls back to unlimited (legacy behaviour).
        assert_eq!(max_stack("res://nope.tres"), u32::MAX);
    }
}
