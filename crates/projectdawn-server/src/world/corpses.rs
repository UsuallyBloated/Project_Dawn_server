//! Server-owned player corpses (corpse / resurrection epic, Slice 1).
//!
//! A corpse is a persisted, owner-only [`super::loot::LootBag`]: on death a
//! player's gear (equipped + bags) and carried coin move onto a corpse that
//! sits where they died, the player respawns naked, and the corpse persists
//! across a server restart (DB-backed, see `db::{save_corpse, load_corpses,
//! delete_corpse}`) and decays harshly after [`CORPSE_LINGER_SECS`] (the row +
//! its items are deleted; unretrieved gear is gone for good).
//!
//! Ids are minted from the loot-bag id partition (`loot::mint_bag_id`) so the
//! client routes despawn / AOI exactly like a loot bag; a corpse is told apart
//! on spawn by the dedicated `CorpseSpawn` message. The boot loader advances the
//! shared `NEXT_BAG_ID` atomic past the max loaded corpse id (see
//! `loot::reserve_bag_ids_through`) so a fresh bag can't reuse a corpse id.

use protocol::world::{Coins, EntityId};
use std::time::Instant;

use super::connection::Vec3f;
use super::loot::LootItemStack;

/// How long a corpse lingers before it decays and its gear is lost for good.
/// Deliberately short for the Slice 1 playtest (so a tester can watch it decay);
/// raise it substantially for production (EQ used tens of minutes to days).
pub const CORPSE_LINGER_SECS: f32 = 300.0; // 5 minutes

/// One server-owned player corpse. Lives in `tick::run`'s
/// `HashMap<EntityId, Corpse>` between creation/boot-load and decay/retrieval.
// `zone` / `items` / `coins` are persisted now but only consumed in Slice 2
// (corpse retrieval) and a future corpse-locate; Slice 1 is render-only.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Corpse {
    pub id: EntityId,
    pub owner_char: i64,
    /// Cached display name for the "<name>'s corpse" nameplate, so a corpse
    /// reloaded at boot can be labelled without a join to the owner.
    pub owner_name: String,
    pub zone: String,
    pub pos: Vec3f,
    pub items: Vec<LootItemStack>,
    pub coins: Coins,
    /// Decay clock. Set to creation time; on a boot-loaded corpse it is reset to
    /// boot time, so a restart restarts the linger rather than losing the corpse.
    pub spawned_at: Instant,
}

impl Corpse {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: EntityId,
        owner_char: i64,
        owner_name: String,
        zone: String,
        pos: Vec3f,
        items: Vec<LootItemStack>,
        coins: Coins,
        now: Instant,
    ) -> Self {
        Self { id, owner_char, owner_name, zone, pos, items, coins, spawned_at: now }
    }
}
