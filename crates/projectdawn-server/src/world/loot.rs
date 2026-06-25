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

use super::groups::GroupManager;
use protocol::world::{Coins, EntityId, LOOT_BAG_ID_BASE};
use rand::Rng;
use renet::ClientId;
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
    /// Coin sitting on the corpse, rolled by mob tier at death. Credited
    /// to the looter (or split among the nearby group) on the first loot
    /// action, then zeroed. See `roll_coin_for_mob`.
    pub coins: Coins,
    /// PD_W0021 — the dead creature's display name, carried so every
    /// re-snapshot (AOI entry, item removed) can re-send it. The client
    /// renders a "<name>'s corpse" body from it instead of a golden orb.
    /// EMPTY for a player-dropped public bag (keeps the old sack visual).
    pub creature_name: String,
    /// The player credited with the kill that dropped this bag (top
    /// damager). Loot rights extend to this player and — resolved at
    /// loot time — their current group. `None` marks a public bag (e.g.
    /// a player-dropped item) that anyone in range may take.
    pub owner_killer: Option<ClientId>,
    /// Round-robin claim. In a Round Robin group the first loot attempt
    /// assigns this corpse to the next eligible member (advancing the
    /// group's turn); only they may take its items. `None` = unclaimed,
    /// or no turn restriction (FFA / solo / public). Coin ignores this.
    pub assigned_looter: Option<ClientId>,
    pub spawned_at: Instant,
}

impl LootBag {
    pub fn new(
        pos: super::connection::Vec3f,
        items: Vec<LootItemStack>,
        coins: Coins,
        creature_name: String,
        owner_killer: Option<ClientId>,
        now: Instant,
    ) -> Self {
        Self {
            id: mint_bag_id(),
            pos,
            items,
            coins,
            creature_name,
            owner_killer,
            assigned_looter: None,
            spawned_at: now,
        }
    }

    /// A bag is empty (and should despawn) only once both its items and
    /// its coin are gone.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.coins == Coins::ZERO
    }

    /// Whether `looter` is allowed to take from this bag. Public bags
    /// (no owner) are open to anyone in range; owned bags are restricted
    /// to the kill-creditor and their current group-mates. Group rights
    /// are resolved live so they follow the group's present membership,
    /// not whoever happened to land the killing blow.
    pub fn can_loot(&self, looter: ClientId, groups: &GroupManager) -> bool {
        match self.owner_killer {
            None => true,
            Some(owner) => looter == owner || groups.same_group(looter, owner),
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

/// Corpse / resurrection Slice 1 — corpses are persisted but mint their ids from
/// this same loot-bag partition, and the atomic resets to `LOOT_BAG_ID_BASE` on
/// restart. After boot-loading corpses, advance the minter past the highest
/// loaded id so a freshly-minted bag/corpse can never reuse a loaded corpse's id.
/// No-op when `max_id + 1` is below the current next.
pub fn reserve_bag_ids_through(max_id: EntityId) {
    NEXT_BAG_ID.fetch_max(max_id + 1, Ordering::Relaxed);
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

// ── Coin drops (see docs/design/group_loot_and_coin.md) ───────────────
//
// Coin is rolled by mob tier on death and stored on the bag, separate
// from the item table. Wildlife/beasts drop none; humanoid + undead
// tiers scale by mob level per docs/concepts/world/currency.md. Amounts
// roll in copper and reduce to minimal coins via Coins::from_copper, so
// a named mob shows "~15 silver", not 1500 raw copper.

/// Case-insensitive substring markers for non-coin-dropping wildlife.
/// Everything else is treated as a humanoid/undead coin-dropper scaled
/// by level. Tunable; undead (skeleton/zombie) deliberately DO drop
/// coin. Uses the same loose substring match as the loot-table lookup,
/// so an off name ("Bearer") can misfire — keep the list specific.
const BEAST_NAMES: &[&str] = &[
    "wolf", "rat", "boar", "snake", "bear", "spider", "bat",
    "crawler", "wasp", "beetle", "lion", "tiger", "scorpion", "drake",
];

pub fn is_beast(mob_name: &str) -> bool {
    let lower = mob_name.to_lowercase();
    BEAST_NAMES.iter().any(|b| lower.contains(b))
}

/// Roll coin for a slain mob by tier. Returns `Coins::ZERO` for beasts.
/// Bands mirror currency.md's Loot Drops table; named/boss are
/// level-approximated until named mobs are flagged server-side.
pub fn roll_coin_for_mob(mob_name: &str, level: u32) -> Coins {
    if is_beast(mob_name) {
        return Coins::ZERO;
    }
    let mut rng = rand::thread_rng();
    let copper: i64 = if level <= 9 {
        // Low humanoid: 5–50c.
        rng.gen_range(5..=50)
    } else if level <= 19 {
        // Mid humanoid: 50–300c, ~20% chance of a little silver.
        let mut c: i64 = rng.gen_range(50..=300);
        if rng.gen_bool(0.20) {
            c += rng.gen_range(1..=3) * 100;
        }
        c
    } else if level <= 29 {
        // Named: 1–20s, ~15% chance of 1–2g.
        let mut c: i64 = rng.gen_range(1..=20) * 100;
        if rng.gen_bool(0.15) {
            c += rng.gen_range(1..=2) * 10_000;
        }
        c
    } else {
        // Boss: 1–10g, ~5% chance of 1p.
        let mut c: i64 = rng.gen_range(1..=10) * 10_000;
        if rng.gen_bool(0.05) {
            c += 1_000_000;
        }
        c
    };
    Coins::from_copper(copper)
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

    fn empty_bag(owner: Option<ClientId>) -> LootBag {
        LootBag::new(
            super::super::connection::Vec3f::ZERO,
            vec![],
            Coins::ZERO,
            String::new(),
            owner,
            Instant::now(),
        )
    }

    #[test]
    fn beasts_drop_no_coin() {
        assert_eq!(roll_coin_for_mob("Dire Wolf", 5), Coins::ZERO);
        assert_eq!(roll_coin_for_mob("Cave Bat", 18), Coins::ZERO);
        assert!(is_beast("Giant Rat"));
        assert!(!is_beast("Gnoll Raider"));
        assert!(!is_beast("Decrepit Skeleton"), "undead drop coin");
    }

    #[test]
    fn low_humanoid_coin_in_band() {
        for _ in 0..100 {
            let c = roll_coin_for_mob("Bandit Scout", 5).total_copper();
            assert!((5..=50).contains(&c), "low-humanoid copper {c} out of band");
        }
    }

    #[test]
    fn mid_humanoid_coin_in_band() {
        for _ in 0..100 {
            let c = roll_coin_for_mob("Gnoll Raider", 15).total_copper();
            // 50–300c base, plus an optional 100–300c silver bonus.
            assert!((50..=600).contains(&c), "mid-humanoid copper {c} out of band");
        }
    }

    #[test]
    fn boss_coin_is_gold_scale() {
        for _ in 0..100 {
            let c = roll_coin_for_mob("Ancient Warlord", 35).total_copper();
            // 1–10g, plus a rare +1p.
            assert!(c >= 10_000, "boss copper {c} below floor");
            assert!(c <= 100_000 + 1_000_000, "boss copper {c} above ceiling");
        }
    }

    #[test]
    fn public_bag_loots_for_anyone() {
        let gm = GroupManager::new();
        let bag = empty_bag(None);
        assert!(bag.can_loot(1, &gm));
        assert!(bag.can_loot(99, &gm));
    }

    #[test]
    fn owned_bag_loots_only_for_owner_when_solo() {
        let gm = GroupManager::new();
        let bag = empty_bag(Some(7));
        assert!(bag.can_loot(7, &gm));
        assert!(!bag.can_loot(8, &gm), "a stranger must not loot a solo kill");
    }

    #[test]
    fn owned_bag_loots_for_group_mates() {
        // 7 invites 8; both end up in one group. A kill by 7 is lootable
        // by 8, but not by an ungrouped stranger (9).
        let mut gm = GroupManager::new();
        gm.record_invite(7, 8);
        assert!(gm.accept(8, 7).is_some());
        let bag = empty_bag(Some(7));
        assert!(bag.can_loot(7, &gm));
        assert!(bag.can_loot(8, &gm), "a group-mate must be able to loot");
        assert!(!bag.can_loot(9, &gm), "a non-member must not loot");
    }
}
