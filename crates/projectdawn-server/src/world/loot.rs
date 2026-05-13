//! Server-side loot tables. Ported from
//! `Project_Dawn/data/loot_tables.gd` (`MobLootTables.TABLES`); the
//! `Project_Dawn` copy stays the runtime source for the client's
//! `ItemData` resources (the `.tres` files), but the rolling logic
//! that decides what drops moves server-side per sub-task 4.
//!
//! Item resolution: each entry's `item_path` is the `res://` path the
//! client `load()`s to get the `ItemData`. The server doesn't know or
//! care about the file contents — it just ships the path string.
//!
//! Mob → table lookup matches the GDScript semantics: exact match
//! first, falls back to a partial substring match (so "Decrepit
//! Skeleton" picks up the "Skeleton" table). Unknown mob names roll
//! the generic fallback table.

use protocol::world::{EntityId, LOOT_BAG_ID_BASE};
use rand::Rng;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct LootEntry {
    pub item_path: &'static str,
    pub weight: f32,
    pub min_count: u32,
    pub max_count: u32,
}

#[derive(Debug, Clone)]
pub struct MobLootTable {
    pub rolls: u32,
    pub empty_weight: f32,
    pub entries: &'static [LootEntry],
}

/// One stack rolled into a live bag. `item_path` is the canonical
/// `res://` reference; the client maps it to an `ItemData` via `load()`.
#[derive(Debug, Clone)]
pub struct LootItemStack {
    pub item_path: String,
    pub count: u32,
}

/// One server-owned loot bag. Lives in `tick::run`'s `HashMap<EntityId,
/// LootBag>` between spawn and despawn. Empty-bag and timer-expired
/// bags get fanned out as `EntityDespawn` and removed.
#[derive(Debug)]
pub struct LootBag {
    pub id: EntityId,
    pub pos: super::connection::Vec3f,
    pub items: Vec<LootItemStack>,
    pub spawned_at: Instant,
}

impl LootBag {
    pub fn new(pos: super::connection::Vec3f, items: Vec<LootItemStack>, now: Instant) -> Self {
        Self {
            id: mint_bag_id(),
            pos,
            items,
            spawned_at: now,
        }
    }

    /// Wire-format conversion for `LootBagSpawn` payloads. The protocol
    /// variant uses `(String, u32)` pairs to keep bincode lean.
    pub fn snapshot(&self) -> Vec<(String, u32)> {
        self.items
            .iter()
            .map(|s| (s.item_path.clone(), s.count))
            .collect()
    }
}

static NEXT_BAG_ID: AtomicU64 = AtomicU64::new(LOOT_BAG_ID_BASE);

pub fn mint_bag_id() -> EntityId {
    NEXT_BAG_ID.fetch_add(1, Ordering::Relaxed)
}

/// Roll a fresh bag's contents for `mob_name`. Returns `None` if the
/// roll produced zero stacks (the empty bucket won for every roll), so
/// the caller can skip the spawn entirely.
pub fn roll_for_mob(mob_name: &str) -> Option<Vec<LootItemStack>> {
    let table = find_table(mob_name)?;
    let mut rng = rand::thread_rng();
    let mut out: Vec<LootItemStack> = Vec::new();
    for _ in 0..table.rolls {
        // Skip rolls where the entry pool is empty (shouldn't happen
        // for any authored table but keeps the loop robust).
        if table.entries.is_empty() {
            continue;
        }
        let total: f32 = table.empty_weight + table.entries.iter().map(|e| e.weight).sum::<f32>();
        let mut r: f32 = rng.gen_range(0.0..total);
        if r < table.empty_weight {
            continue;
        }
        r -= table.empty_weight;
        for entry in table.entries {
            if r < entry.weight {
                let count = rng.gen_range(entry.min_count..=entry.max_count.max(entry.min_count));
                out.push(LootItemStack {
                    item_path: entry.item_path.into(),
                    count,
                });
                break;
            }
            r -= entry.weight;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn find_table(mob_name: &str) -> Option<&'static MobLootTable> {
    let lower = mob_name.to_lowercase();
    // Exact match first.
    for (key, table) in TABLES {
        if key.eq_ignore_ascii_case(mob_name) {
            return Some(table);
        }
    }
    // Partial substring match (matches the GDScript `containsn`
    // semantics — case-insensitive contains in either direction).
    for (key, table) in TABLES {
        let key_lower = key.to_lowercase();
        if lower.contains(&key_lower) || key_lower.contains(&lower) {
            return Some(table);
        }
    }
    None
}

// ── Authored tables (mirror of MobLootTables.TABLES) ──────────────────

const T_WOLF: MobLootTable = MobLootTable {
    rolls: 1,
    empty_weight: 0.5,
    entries: &[
        LootEntry { item_path: "res://data/loot/items/wolf_meat.tres",         weight: 3.0, min_count: 1, max_count: 2 },
        LootEntry { item_path: "res://data/loot/items/damaged_wolf_pelt.tres", weight: 2.0, min_count: 1, max_count: 1 },
        LootEntry { item_path: "res://data/loot/items/fresh_wolf_pelt.tres",   weight: 1.0, min_count: 1, max_count: 1 },
        LootEntry { item_path: "res://data/loot/items/sinew.tres",             weight: 2.0, min_count: 1, max_count: 2 },
    ],
};

const T_SKELETON: MobLootTable = MobLootTable {
    rolls: 1,
    empty_weight: 1.0,
    entries: &[
        LootEntry { item_path: "res://data/loot/items/bone_fragment.tres", weight: 3.0, min_count: 1, max_count: 2 },
        LootEntry { item_path: "res://data/loot/items/cloth_scraps.tres",  weight: 1.5, min_count: 1, max_count: 2 },
    ],
};

const T_GNOLL: MobLootTable = MobLootTable {
    rolls: 1,
    empty_weight: 0.5,
    entries: &[
        LootEntry { item_path: "res://data/loot/items/cloth_scraps.tres", weight: 3.0, min_count: 1, max_count: 3 },
        LootEntry { item_path: "res://data/loot/items/gnoll_meat.tres",   weight: 2.0, min_count: 1, max_count: 1 },
        LootEntry { item_path: "res://data/loot/items/gnoll_tooth.tres",  weight: 1.5, min_count: 1, max_count: 2 },
        LootEntry { item_path: "res://data/loot/items/sinew.tres",        weight: 1.0, min_count: 1, max_count: 1 },
    ],
};

const T_RAT: MobLootTable = MobLootTable {
    rolls: 1,
    empty_weight: 1.0,
    entries: &[
        LootEntry { item_path: "res://data/loot/items/rat_meat.tres",      weight: 3.0, min_count: 1, max_count: 1 },
        LootEntry { item_path: "res://data/loot/items/bone_fragment.tres", weight: 1.0, min_count: 1, max_count: 1 },
    ],
};

const T_BOAR: MobLootTable = MobLootTable {
    rolls: 1,
    empty_weight: 0.5,
    entries: &[
        LootEntry { item_path: "res://data/loot/items/boar_hide.tres", weight: 2.5, min_count: 1, max_count: 1 },
        LootEntry { item_path: "res://data/loot/items/wolf_meat.tres", weight: 2.0, min_count: 1, max_count: 2 },
        LootEntry { item_path: "res://data/loot/items/sinew.tres",     weight: 1.5, min_count: 1, max_count: 2 },
    ],
};

const T_SNAKE: MobLootTable = MobLootTable {
    rolls: 1,
    empty_weight: 0.5,
    entries: &[
        LootEntry { item_path: "res://data/loot/items/snake_skin.tres",      weight: 2.5, min_count: 1, max_count: 1 },
        LootEntry { item_path: "res://data/loot/items/snake_meat.tres",      weight: 2.0, min_count: 1, max_count: 1 },
        LootEntry { item_path: "res://data/loot/items/snake_venom_sac.tres", weight: 0.8, min_count: 1, max_count: 1 },
    ],
};

const T_BEAR: MobLootTable = MobLootTable {
    rolls: 2,
    empty_weight: 0.3,
    entries: &[
        LootEntry { item_path: "res://data/loot/items/bear_hide.tres", weight: 2.5, min_count: 1, max_count: 1 },
        LootEntry { item_path: "res://data/loot/items/wolf_meat.tres", weight: 2.5, min_count: 2, max_count: 3 },
        LootEntry { item_path: "res://data/loot/items/sinew.tres",     weight: 2.0, min_count: 2, max_count: 3 },
    ],
};

const T_SPIDER: MobLootTable = MobLootTable {
    rolls: 1,
    empty_weight: 0.5,
    entries: &[
        LootEntry { item_path: "res://data/loot/items/spiderling_silk.tres",  weight: 2.5, min_count: 1, max_count: 2 },
        LootEntry { item_path: "res://data/loot/items/spider_venom_sac.tres", weight: 1.0, min_count: 1, max_count: 1 },
    ],
};

const T_BAT: MobLootTable = MobLootTable {
    rolls: 1,
    empty_weight: 1.0,
    entries: &[
        LootEntry { item_path: "res://data/loot/items/bat_blood.tres", weight: 2.0, min_count: 1, max_count: 1 },
        LootEntry { item_path: "res://data/loot/items/bat_wing.tres",  weight: 1.5, min_count: 1, max_count: 2 },
    ],
};

const T_ZOMBIE: MobLootTable = MobLootTable {
    rolls: 1,
    empty_weight: 1.0,
    entries: &[
        LootEntry { item_path: "res://data/loot/items/cloth_scraps.tres",  weight: 3.0, min_count: 1, max_count: 3 },
        LootEntry { item_path: "res://data/loot/items/bone_fragment.tres", weight: 2.0, min_count: 1, max_count: 2 },
    ],
};

const T_BANDIT: MobLootTable = MobLootTable {
    rolls: 2,
    empty_weight: 0.5,
    entries: &[
        LootEntry { item_path: "res://data/loot/items/cloth_scraps.tres", weight: 3.0, min_count: 1, max_count: 3 },
        LootEntry { item_path: "res://data/loot/items/copper_ore.tres",   weight: 1.0, min_count: 1, max_count: 2 },
        LootEntry { item_path: "res://data/loot/items/metal_bits.tres",   weight: 1.5, min_count: 1, max_count: 3 },
        LootEntry { item_path: "res://data/loot/items/coal.tres",         weight: 1.0, min_count: 1, max_count: 2 },
    ],
};

const TABLES: &[(&str, &MobLootTable)] = &[
    ("Wolf",     &T_WOLF),
    ("Skeleton", &T_SKELETON),
    ("Gnoll",    &T_GNOLL),
    ("Rat",      &T_RAT),
    ("Boar",     &T_BOAR),
    ("Snake",    &T_SNAKE),
    ("Bear",     &T_BEAR),
    ("Spider",   &T_SPIDER),
    ("Bat",      &T_BAT),
    ("Zombie",   &T_ZOMBIE),
    ("Bandit",   &T_BANDIT),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_partial_match_resolves() {
        // Starter zone has "Decrepit Skeleton" and "Rotting Skeleton".
        // Both must resolve to the Skeleton table via partial match.
        assert!(find_table("Decrepit Skeleton").is_some());
        assert!(find_table("Rotting Skeleton").is_some());
        assert!(find_table("Skeleton").is_some());
    }

    #[test]
    fn bandit_match_resolves() {
        assert!(find_table("Bandit Scout").is_some());
    }

    #[test]
    fn unknown_mob_returns_no_loot() {
        assert!(roll_for_mob("Unknown Test Mob").is_none());
    }

    #[test]
    fn minted_bag_ids_partition_above_enemies() {
        let a = mint_bag_id();
        let b = mint_bag_id();
        assert!(a >= LOOT_BAG_ID_BASE);
        assert!(b > a);
    }
}
