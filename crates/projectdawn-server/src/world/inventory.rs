//! Track 13.1 — server-side player inventory.
//!
//! The in-memory shape of what each player owns. For Track 13.1 only
//! the base 8-slot row matters; bags-as-items and the paperdoll come
//! in 13.2 / 13.3.
//!
//! Today the client is still authoritative for slot-to-slot moves;
//! the server's view is informational — populated by loot grants
//! server-side, persisted on disconnect, but not yet driving the
//! wire snapshot the client renders from. 13.2 takes that step;
//! this module is the shape it converges to.

use crate::db::InventoryRow;
use crate::world::items;
use std::collections::HashMap;

/// Track 14.2 — running totals of stat bonuses contributed by the
/// currently-equipped item set. Cached on `PerConnection` so the
/// recompute pass knows what to subtract before re-summing across
/// the new equipment map. Kept distinct from buff deltas so spells
/// like Bless can co-exist with equipment changes without either
/// stomping the other.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EquipStatBonuses {
    pub strength: i32,
    pub dexterity: i32,
    pub agility: i32,
    pub intelligence: i32,
    pub wisdom: i32,
    pub charisma: i32,
    pub constitution: i32,
    pub max_hp: f32,
    pub max_mp: f32,
    pub max_stamina: f32,
    pub armor: i32,
}

/// Track 14.2 — outcome flags from `recompute_equipped_stats`. The
/// caller fans `HealthUpdate` / `ManaUpdate` / `StaminaUpdate` when
/// the corresponding max moved so peers' target frames refresh.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RecomputeResult {
    pub max_hp_changed: bool,
    pub max_mp_changed: bool,
    pub max_stamina_changed: bool,
}

impl RecomputeResult {
    pub fn any_resource_max_changed(&self) -> bool {
        self.max_hp_changed || self.max_mp_changed || self.max_stamina_changed
    }
}

/// Track 14.2 — rebuild the effective stats / max resources / armor
/// from the connection's equipment map. Diffs the new gear total
/// against `conn.equip_stat_bonuses` and applies the signed delta
/// directly to `conn.{strength, ..., max_hp, max_mp, max_stamina,
/// equipped_armor}`. Current hp / mp / stamina are clamped against
/// the new max when the max moves down (mirrors GDScript
/// `set_hp(hp)` after `remove_item_bonuses`); current resources are
/// NOT bumped when the max moves up — the player has to heal back
/// into the new headroom, matching `apply_item_bonuses` which only
/// touches `max_*` not the current value.
///
/// Items not in the registry contribute nothing — they're already
/// rejected by `equip_from_base` (Track 14.1), so any orphaned
/// path that slipped in via persistence simply leaves the bonuses
/// at their prior state. Floors: max_hp >= 1, max_mp >= 0, max_stamina >= 1.
pub fn recompute_equipped_stats(
    conn: &mut crate::world::connection::PerConnection,
) -> RecomputeResult {
    let prev = conn.equip_stat_bonuses;
    let mut next = EquipStatBonuses::default();
    for entry in conn.inventory.equipment.values() {
        let Some(item) = items::lookup(&entry.item_path) else {
            continue;
        };
        next.strength     += item.str_bonus;
        next.dexterity    += item.dex_bonus;
        next.agility      += item.agi_bonus;
        next.intelligence += item.int_bonus;
        next.wisdom       += item.wis_bonus;
        next.charisma     += item.cha_bonus;
        next.constitution += item.con_bonus;
        next.max_hp       += item.max_hp_bonus;
        next.max_mp       += item.max_mp_bonus;
        next.max_stamina  += item.max_stamina_bonus;
        next.armor        += item.armor;
    }
    // Stats — straight signed-delta apply.
    conn.strength     += next.strength     - prev.strength;
    conn.dexterity    += next.dexterity    - prev.dexterity;
    conn.agility      += next.agility      - prev.agility;
    conn.intelligence += next.intelligence - prev.intelligence;
    conn.wisdom       += next.wisdom       - prev.wisdom;
    conn.charisma     += next.charisma     - prev.charisma;
    conn.constitution += next.constitution - prev.constitution;

    // Resource maxes — signed delta + floor. Current is clamped down
    // if max dropped below it.
    let prev_max_hp = conn.max_hp;
    let prev_max_mp = conn.max_mp;
    let prev_max_stamina = conn.max_stamina;
    conn.max_hp      = (conn.max_hp      + (next.max_hp      - prev.max_hp     )).max(1.0);
    conn.max_mp      = (conn.max_mp      + (next.max_mp      - prev.max_mp     )).max(0.0);
    conn.max_stamina = (conn.max_stamina + (next.max_stamina - prev.max_stamina)).max(1.0);
    if conn.hp > conn.max_hp {
        conn.hp = conn.max_hp;
    }
    if conn.mp > conn.max_mp {
        conn.mp = conn.max_mp;
    }
    if conn.stamina > conn.max_stamina {
        conn.stamina = conn.max_stamina;
    }

    conn.equipped_armor = next.armor.max(0);
    conn.equip_stat_bonuses = next;

    RecomputeResult {
        max_hp_changed:      conn.max_hp      != prev_max_hp,
        max_mp_changed:      conn.max_mp      != prev_max_mp,
        max_stamina_changed: conn.max_stamina != prev_max_stamina,
    }
}

/// Mirror of the client's `Inventory.BASE_SLOT_COUNT` constant. The
/// 8 flat slots a player has before bags. The full inventory model
/// adds per-bag rows (Track 13.2) and equipment (Track 13.3); the
/// server tracks all three under different `location` strings.
pub const BASE_SLOT_COUNT: usize = 8;

/// Track 13.3 — paperdoll slot count. Matches the client's
/// `Equipment.SLOTS` array length (weapon, offhand, head, chest,
/// legs, feet, hands, ring, neck) and the
/// `protocol::world::EquipSlot` enum order.
pub const EQUIP_SLOT_COUNT: u8 = 9;

#[derive(Debug, Clone, PartialEq)]
pub struct InventoryEntry {
    pub item_path: String,
    pub count: u32,
}

#[derive(Debug)]
pub struct PlayerInventory {
    /// 8 base slots, parallel to the client's `Inventory.base_slots`.
    /// `None` is an empty slot.
    pub base: Vec<Option<InventoryEntry>>,
    /// Track 13.3 — paperdoll. Sparse map keyed by equip-slot index;
    /// only occupied slots have entries. Item-vs-slot validation
    /// (e.g. "this is a weapon, not a helm") is deferred until the
    /// server-side item registry lands; today the server just
    /// trusts the client's chosen equip_slot index after range-
    /// checking it against EQUIP_SLOT_COUNT.
    pub equipment: HashMap<u8, InventoryEntry>,
}

impl Default for PlayerInventory {
    fn default() -> Self {
        Self {
            base: vec![None; BASE_SLOT_COUNT],
            equipment: HashMap::new(),
        }
    }
}

impl PlayerInventory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconstruct from DB rows. Handles 'base' (Track 13.1) and
    /// 'equip' (Track 13.3); 'bag_*' rows are still dropped (bag
    /// support needs the server-side item registry). Out-of-range
    /// slots are dropped defensively.
    pub fn from_rows(rows: &[InventoryRow]) -> Self {
        let mut inv = Self::default();
        for row in rows {
            if row.count <= 0 || row.item_path.is_empty() {
                continue;
            }
            match row.location.as_str() {
                "base" => {
                    let idx = row.slot as usize;
                    if idx >= BASE_SLOT_COUNT {
                        continue;
                    }
                    inv.base[idx] = Some(InventoryEntry {
                        item_path: row.item_path.clone(),
                        count: row.count as u32,
                    });
                }
                "equip" => {
                    if row.slot < 0 || row.slot >= EQUIP_SLOT_COUNT as i32 {
                        continue;
                    }
                    inv.equipment.insert(
                        row.slot as u8,
                        InventoryEntry {
                            item_path: row.item_path.clone(),
                            count: row.count as u32,
                        },
                    );
                }
                _ => {
                    // 'bag_*' deferred to a later track.
                }
            }
        }
        inv
    }

    /// Project into DB rows for persistence. Only emits filled slots.
    /// Emits 'base' rows + Track 13.3's 'equip' rows.
    pub fn to_rows(&self) -> Vec<InventoryRow> {
        let mut out = Vec::new();
        for (i, slot) in self.base.iter().enumerate() {
            if let Some(entry) = slot {
                out.push(InventoryRow {
                    location: "base".to_string(),
                    slot: i as i32,
                    item_path: entry.item_path.clone(),
                    count: entry.count as i32,
                });
            }
        }
        for (slot, entry) in self.equipment.iter() {
            out.push(InventoryRow {
                location: "equip".to_string(),
                slot: *slot as i32,
                item_path: entry.item_path.clone(),
                count: entry.count as i32,
            });
        }
        out
    }

    /// Track 13.2 — project to the wire shape used by
    /// `ServerWorldMsg::InventorySnapshot`. Parallel to `to_rows`
    /// but emits the protocol's (location, slot, item_path, count)
    /// tuple. Includes Track 13.3 equipment entries.
    pub fn to_snapshot_entries(&self) -> Vec<(String, u32, String, u32)> {
        let mut out = Vec::new();
        for (i, slot) in self.base.iter().enumerate() {
            if let Some(entry) = slot {
                out.push((
                    "base".to_string(),
                    i as u32,
                    entry.item_path.clone(),
                    entry.count,
                ));
            }
        }
        for (slot, entry) in self.equipment.iter() {
            out.push((
                "equip".to_string(),
                *slot as u32,
                entry.item_path.clone(),
                entry.count,
            ));
        }
        out
    }

    /// Track 13.2 / 14.1 — drop `count` of `item_path` into the
    /// inventory, respecting per-item `max_stack` from the registry.
    ///
    /// Behaviour:
    /// * First tops up any existing same-item stacks up to `max_stack`.
    /// * Then claims empty slots, each starting a fresh stack capped
    ///   at `max_stack`.
    /// * Returns `(touched_slots, leftover)` where `leftover` is the
    ///   amount that didn't fit. Empty `touched_slots` + nonzero
    ///   `leftover` means nothing was placed (inventory full).
    ///
    /// The leftover gives loot a refund path — the caller can push
    /// the remainder back into the loot bag rather than losing it.
    /// Unknown item paths (no registry entry) use `u32::MAX` for
    /// `max_stack`, preserving the legacy unlimited-stack behaviour
    /// for runtime-built items or anything not in items.toml yet.
    pub fn add_item_locating(
        &mut self,
        item_path: &str,
        count: u32,
    ) -> Result<(Vec<usize>, u32), &'static str> {
        if count == 0 {
            return Err("zero count");
        }
        if item_path.is_empty() {
            return Err("empty item_path");
        }
        let cap = items::max_stack(item_path);
        let mut remaining = count;
        let mut touched: Vec<usize> = Vec::new();
        // Pass 1 — top up existing stacks with the same item.
        for i in 0..BASE_SLOT_COUNT {
            if remaining == 0 {
                break;
            }
            if let Some(entry) = self.base[i].as_mut() {
                if entry.item_path == item_path && entry.count < cap {
                    let space = cap - entry.count;
                    let put = remaining.min(space);
                    entry.count = entry.count.saturating_add(put);
                    remaining -= put;
                    touched.push(i);
                }
            }
        }
        // Pass 2 — claim empty slots, each capped at max_stack.
        for i in 0..BASE_SLOT_COUNT {
            if remaining == 0 {
                break;
            }
            if self.base[i].is_none() {
                let put = remaining.min(cap);
                self.base[i] = Some(InventoryEntry {
                    item_path: item_path.to_string(),
                    count: put,
                });
                remaining -= put;
                touched.push(i);
            }
        }
        Ok((touched, remaining))
    }

    /// Track 13.2.b — split `count` items off src into dst. Dst must
    /// be empty or hold the same item_path (merge); different
    /// item_paths are rejected (the legacy UI uses MoveItem for
    /// swaps). Src is reduced by `count`; if that empties it, the
    /// slot is cleared. Returns the touched slots so the caller
    /// fans one `InventoryDelta` per slot.
    pub fn split_base(
        &mut self,
        src: usize,
        dst: usize,
        count: u32,
    ) -> Result<Vec<usize>, &'static str> {
        if src >= BASE_SLOT_COUNT || dst >= BASE_SLOT_COUNT {
            return Err("slot out of range");
        }
        if src == dst {
            return Err("src == dst");
        }
        if count == 0 {
            return Err("zero count");
        }
        let src_entry = self
            .base
            .get(src)
            .and_then(|s| s.as_ref())
            .ok_or("source slot empty")?;
        if src_entry.count < count {
            return Err("source has fewer items than split count");
        }
        let src_path = src_entry.item_path.clone();
        // Validate dst before mutating either slot.
        match self.base.get(dst).and_then(|s| s.as_ref()) {
            None => {} // empty dst — clean transfer.
            Some(existing) if existing.item_path == src_path => {} // merge.
            Some(_) => return Err("dst holds a different item"),
        }
        // Apply: subtract from src (clear if zero) then add to dst.
        let src_now_zero;
        {
            let src_entry_mut = self.base[src].as_mut().expect("checked");
            src_entry_mut.count -= count;
            src_now_zero = src_entry_mut.count == 0;
        }
        if src_now_zero {
            self.base[src] = None;
        }
        match self.base[dst].as_mut() {
            Some(existing) => {
                existing.count = existing.count.saturating_add(count);
            }
            None => {
                self.base[dst] = Some(InventoryEntry {
                    item_path: src_path,
                    count,
                });
            }
        }
        Ok(vec![src, dst])
    }

    /// Track 13.3 / 14.1 — equip the entry at base slot `src` into
    /// paperdoll slot `equip_slot`. If the paperdoll already holds
    /// an item, swap it back into the source slot. Returns the
    /// list of (location, slot) tuples touched so the caller fans
    /// one `InventoryDelta` per slot.
    ///
    /// Track 14.1 — validates that the source item's type matches
    /// the chosen paperdoll slot via `items::is_equippable_in_slot`.
    /// Wrong-slot equips reject before any mutation. Item paths not
    /// in the registry reject too (the server only equips items it
    /// has authored data for).
    pub fn equip_from_base(
        &mut self,
        src: usize,
        equip_slot: u8,
    ) -> Result<Vec<(&'static str, u32)>, &'static str> {
        if src >= BASE_SLOT_COUNT {
            return Err("base slot out of range");
        }
        if equip_slot >= EQUIP_SLOT_COUNT {
            return Err("equip slot out of range");
        }
        // Inspect the source item first; reject early on type
        // mismatch so we don't have to roll the swap back.
        let src_path = match self.base[src].as_ref() {
            Some(e) => e.item_path.clone(),
            None => return Err("source slot empty"),
        };
        if !items::is_equippable_in_slot(&src_path, equip_slot) {
            return Err("item not equippable in this slot");
        }
        let src_entry = self.base[src].take().expect("checked above");
        let prev_equip = self.equipment.remove(&equip_slot);
        self.equipment.insert(equip_slot, src_entry);
        if let Some(prev) = prev_equip {
            // Swap the previously equipped item back into the now-
            // empty src slot.
            self.base[src] = Some(prev);
        }
        Ok(vec![("base", src as u32), ("equip", equip_slot as u32)])
    }

    /// Track 13.3 — unequip paperdoll slot `equip_slot` into base
    /// slot `dst`. If dst is occupied, swap it into the paperdoll
    /// (same byte-range validation as equip; item-vs-slot validation
    /// is deferred).
    pub fn unequip_to_base(
        &mut self,
        equip_slot: u8,
        dst: usize,
    ) -> Result<Vec<(&'static str, u32)>, &'static str> {
        if equip_slot >= EQUIP_SLOT_COUNT {
            return Err("equip slot out of range");
        }
        if dst >= BASE_SLOT_COUNT {
            return Err("base slot out of range");
        }
        let Some(equip_entry) = self.equipment.remove(&equip_slot) else {
            return Err("equip slot empty");
        };
        let prev_base = self.base[dst].take();
        self.base[dst] = Some(equip_entry);
        if let Some(prev) = prev_base {
            self.equipment.insert(equip_slot, prev);
        }
        Ok(vec![("base", dst as u32), ("equip", equip_slot as u32)])
    }

    /// Track 13.2.b — remove `count` of the entry at `(base, slot)`.
    /// `count == 0` drops the whole stack. Returns the item_path
    /// and actual quantity removed (capped by the stack), plus
    /// whether the slot is now empty. None if the slot was already
    /// empty.
    pub fn drop_base(&mut self, slot: usize, count: u32) -> Option<(String, u32)> {
        if slot >= BASE_SLOT_COUNT {
            return None;
        }
        let entry = self.base.get_mut(slot)?.as_mut()?;
        let path = entry.item_path.clone();
        let to_remove = if count == 0 || count >= entry.count {
            entry.count
        } else {
            count
        };
        entry.count -= to_remove;
        if entry.count == 0 {
            self.base[slot] = None;
        }
        Some((path, to_remove))
    }

    /// Track 13.2 — atomic move/swap between base slots. Move-to-empty
    /// is a clean transfer; move-to-occupied with the same item_path
    /// merges counts (caps at u32::MAX); move-to-occupied with a
    /// different item_path is a swap. Returns the list of slots
    /// touched so the caller fans one `InventoryDelta` per slot.
    pub fn move_base(&mut self, src: usize, dst: usize) -> Result<Vec<usize>, &'static str> {
        if src >= BASE_SLOT_COUNT || dst >= BASE_SLOT_COUNT {
            return Err("slot out of range");
        }
        if src == dst {
            return Ok(Vec::new());
        }
        let src_entry = self.base[src].take();
        let Some(src_entry) = src_entry else {
            return Err("source slot empty");
        };
        let dst_entry = self.base[dst].take();
        match dst_entry {
            None => {
                self.base[dst] = Some(src_entry);
            }
            Some(existing) if existing.item_path == src_entry.item_path => {
                let merged_count = existing.count.saturating_add(src_entry.count);
                self.base[dst] = Some(InventoryEntry {
                    item_path: existing.item_path,
                    count: merged_count,
                });
            }
            Some(existing) => {
                // Different item — swap. Src now holds what was in dst.
                self.base[src] = Some(existing);
                self.base[dst] = Some(src_entry);
            }
        }
        Ok(vec![src, dst])
    }

    /// Convenience wrapper used by the unit tests below. Returns
    /// `Err("inventory full")` if any of `count` failed to land
    /// (partial mutations are NOT rolled back — call
    /// `add_item_locating` directly if you need finer control over
    /// the leftover).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn add_item(&mut self, item_path: &str, count: u32) -> Result<(), &'static str> {
        if count == 0 {
            return Ok(());
        }
        let (_, leftover) = self.add_item_locating(item_path, count)?;
        if leftover > 0 {
            return Err("inventory full");
        }
        Ok(())
    }

    /// Track 13.2 will consume this when validating drop / split
    /// intents (server has to confirm the player actually has the
    /// claimed quantity before authorizing the move).
    #[allow(dead_code)]
    pub fn total_count_of(&self, item_path: &str) -> u32 {
        self.base
            .iter()
            .filter_map(|s| s.as_ref())
            .filter(|e| e.item_path == item_path)
            .map(|e| e.count)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_eight_empty_slots() {
        let inv = PlayerInventory::new();
        assert_eq!(inv.base.len(), BASE_SLOT_COUNT);
        assert!(inv.base.iter().all(|s| s.is_none()));
    }

    #[test]
    fn add_item_uses_first_empty_slot() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 3).expect("first add");
        assert_eq!(inv.base[0].as_ref().unwrap().item_path, "res://items/cloth.tres");
        assert_eq!(inv.base[0].as_ref().unwrap().count, 3);
        assert!(inv.base[1].is_none());
    }

    #[test]
    fn add_item_stacks_same_path() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 3).unwrap();
        inv.add_item("res://items/cloth.tres", 5).unwrap();
        assert_eq!(inv.total_count_of("res://items/cloth.tres"), 8);
        assert!(inv.base[1].is_none(), "second add stacks; doesn't claim a new slot");
    }

    #[test]
    fn add_item_picks_fresh_slot_when_different() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 3).unwrap();
        inv.add_item("res://items/iron.tres", 2).unwrap();
        assert_eq!(inv.base[0].as_ref().unwrap().item_path, "res://items/cloth.tres");
        assert_eq!(inv.base[1].as_ref().unwrap().item_path, "res://items/iron.tres");
    }

    #[test]
    fn add_item_errors_when_full_with_all_distinct_paths() {
        let mut inv = PlayerInventory::new();
        for i in 0..BASE_SLOT_COUNT {
            inv.add_item(&format!("res://items/i{i}.tres"), 1).unwrap();
        }
        let err = inv.add_item("res://items/overflow.tres", 1);
        assert!(err.is_err());
    }

    #[test]
    fn roundtrip_via_rows() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 4).unwrap();
        inv.add_item("res://items/iron.tres", 7).unwrap();
        let rows = inv.to_rows();
        assert_eq!(rows.len(), 2);
        let restored = PlayerInventory::from_rows(&rows);
        assert_eq!(
            restored.base[0].as_ref().unwrap().item_path,
            "res://items/cloth.tres"
        );
        assert_eq!(restored.base[0].as_ref().unwrap().count, 4);
        assert_eq!(restored.base[1].as_ref().unwrap().count, 7);
    }

    #[test]
    fn split_base_carves_off_count() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 10).unwrap();
        let touched = inv.split_base(0, 5, 3).expect("split");
        assert_eq!(touched, vec![0, 5]);
        assert_eq!(inv.base[0].as_ref().unwrap().count, 7);
        assert_eq!(inv.base[5].as_ref().unwrap().item_path, "res://items/cloth.tres");
        assert_eq!(inv.base[5].as_ref().unwrap().count, 3);
    }

    #[test]
    fn split_base_into_same_path_merges() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 10).unwrap();
        // Manually place a second stack of cloth at slot 2.
        inv.base[2] = Some(InventoryEntry {
            item_path: "res://items/cloth.tres".into(),
            count: 4,
        });
        inv.split_base(0, 2, 3).expect("split merges");
        assert_eq!(inv.base[2].as_ref().unwrap().count, 7);
        assert_eq!(inv.base[0].as_ref().unwrap().count, 7);
    }

    #[test]
    fn split_base_rejects_different_item_at_dst() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 5).unwrap();
        inv.add_item("res://items/iron.tres", 2).unwrap();
        let err = inv.split_base(0, 1, 1);
        assert!(err.is_err(), "different item paths must reject split");
        assert_eq!(inv.base[0].as_ref().unwrap().count, 5, "src unchanged on reject");
        assert_eq!(inv.base[1].as_ref().unwrap().count, 2, "dst unchanged on reject");
    }

    #[test]
    fn split_base_full_carve_empties_src() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 5).unwrap();
        inv.split_base(0, 7, 5).expect("split-all");
        assert!(inv.base[0].is_none(), "src empty after full carve");
        assert_eq!(inv.base[7].as_ref().unwrap().count, 5);
    }

    #[test]
    fn drop_base_partial_keeps_residual() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 10).unwrap();
        let (path, removed) = inv.drop_base(0, 3).expect("drop");
        assert_eq!(path, "res://items/cloth.tres");
        assert_eq!(removed, 3);
        assert_eq!(inv.base[0].as_ref().unwrap().count, 7);
    }

    #[test]
    fn drop_base_zero_count_drops_whole_stack() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 10).unwrap();
        let (_, removed) = inv.drop_base(0, 0).expect("drop whole");
        assert_eq!(removed, 10);
        assert!(inv.base[0].is_none());
    }

    #[test]
    fn drop_base_count_exceeds_stack_drops_all() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 5).unwrap();
        let (_, removed) = inv.drop_base(0, 99).expect("drop over-cap");
        assert_eq!(removed, 5, "drop caps at the stack size");
        assert!(inv.base[0].is_none());
    }

    #[test]
    fn drop_base_empty_slot_returns_none() {
        let mut inv = PlayerInventory::new();
        assert!(inv.drop_base(0, 1).is_none());
    }

    #[test]
    fn from_rows_handles_base_and_equip_skips_bag() {
        let rows = vec![
            InventoryRow {
                location: "base".into(),
                slot: 0,
                item_path: "res://items/cloth.tres".into(),
                count: 5,
            },
            InventoryRow {
                location: "bag_3".into(),
                slot: 0,
                item_path: "res://items/iron.tres".into(),
                count: 2,
            },
            InventoryRow {
                location: "equip".into(),
                slot: 4,
                item_path: "res://items/sword.tres".into(),
                count: 1,
            },
        ];
        let inv = PlayerInventory::from_rows(&rows);
        assert_eq!(inv.total_count_of("res://items/cloth.tres"), 5);
        assert_eq!(inv.total_count_of("res://items/iron.tres"), 0, "bag_* still skipped");
        assert_eq!(
            inv.equipment.get(&4).map(|e| e.item_path.as_str()),
            Some("res://items/sword.tres"),
            "Track 13.3 honours 'equip' rows"
        );
    }

    // Track 14.1 — these tests use real registered paths so the new
    // item-vs-slot validation accepts the equip. Synthetic paths are
    // rejected by `is_equippable_in_slot` (unknown items can't go in
    // the paperdoll).
    const SWORD: &str = "res://data/loot/items/iron_short_sword.tres";
    const ROBE: &str = "res://data/loot/items/cloth_robe.tres";
    const POTION: &str = "res://data/loot/items/minor_healing_potion.tres";

    #[test]
    fn equip_moves_from_base_to_paperdoll() {
        let mut inv = PlayerInventory::new();
        inv.add_item(SWORD, 1).unwrap();
        let touched = inv.equip_from_base(0, 0).expect("equip");
        assert_eq!(touched, vec![("base", 0u32), ("equip", 0u32)]);
        assert!(inv.base[0].is_none());
        assert_eq!(inv.equipment.get(&0).unwrap().item_path, SWORD);
    }

    #[test]
    fn equip_swaps_with_existing_equipped() {
        let mut inv = PlayerInventory::new();
        inv.equipment.insert(
            0,
            InventoryEntry { item_path: "res://data/loot/items/iron_dagger.tres".into(), count: 1 },
        );
        inv.add_item(SWORD, 1).unwrap();
        inv.equip_from_base(0, 0).expect("equip-swap");
        assert_eq!(inv.equipment.get(&0).unwrap().item_path, SWORD);
        assert_eq!(
            inv.base[0].as_ref().unwrap().item_path,
            "res://data/loot/items/iron_dagger.tres",
            "old item returns to src slot"
        );
    }

    #[test]
    fn equip_rejects_empty_source() {
        let mut inv = PlayerInventory::new();
        let err = inv.equip_from_base(0, 0);
        assert!(err.is_err());
    }

    #[test]
    fn equip_rejects_out_of_range_slots() {
        let mut inv = PlayerInventory::new();
        inv.add_item(SWORD, 1).unwrap();
        assert!(inv.equip_from_base(0, EQUIP_SLOT_COUNT).is_err());
        assert!(inv.equip_from_base(BASE_SLOT_COUNT, 0).is_err());
    }

    #[test]
    fn equip_rejects_wrong_slot_type() {
        // Cloth robe (chest, slot 3) into weapon slot (0) must reject
        // without mutating either slot.
        let mut inv = PlayerInventory::new();
        inv.add_item(ROBE, 1).unwrap();
        let err = inv.equip_from_base(0, 0);
        assert!(err.is_err(), "robe into weapon slot must reject");
        assert_eq!(
            inv.base[0].as_ref().unwrap().item_path,
            ROBE,
            "source unchanged on reject"
        );
        assert!(inv.equipment.get(&0).is_none(), "paperdoll unchanged on reject");
    }

    #[test]
    fn equip_rejects_consumable_in_any_slot() {
        // Health potions can't be equipped anywhere.
        let mut inv = PlayerInventory::new();
        inv.add_item(POTION, 1).unwrap();
        for slot in 0..EQUIP_SLOT_COUNT {
            assert!(inv.equip_from_base(0, slot).is_err());
        }
        assert_eq!(inv.base[0].as_ref().unwrap().item_path, POTION);
    }

    #[test]
    fn equip_rejects_unknown_item_path() {
        // Track 14.1 hardening — items not in the registry can't be
        // equipped. Closes the door on a client claiming a fake item
        // path is a valid weapon.
        let mut inv = PlayerInventory::new();
        inv.base[0] = Some(InventoryEntry {
            item_path: "res://items/forged_lies.tres".into(),
            count: 1,
        });
        assert!(inv.equip_from_base(0, 0).is_err());
    }

    #[test]
    fn add_item_caps_at_max_stack_and_spills() {
        // Minor healing potion has stack_size=10. Adding 15 should
        // fill slot 0 to 10, slot 1 to 5.
        let mut inv = PlayerInventory::new();
        let (touched, leftover) = inv
            .add_item_locating(POTION, 15)
            .expect("add 15 potions");
        assert_eq!(leftover, 0);
        assert_eq!(touched, vec![0, 1]);
        assert_eq!(inv.base[0].as_ref().unwrap().count, 10);
        assert_eq!(inv.base[1].as_ref().unwrap().count, 5);
    }

    #[test]
    fn add_item_returns_leftover_when_inventory_caps_out() {
        // Fill 8 slots with 10 potions each = 80 total at cap.
        // Adding 5 more should land 0, leftover 5.
        let mut inv = PlayerInventory::new();
        let (touched, leftover) = inv.add_item_locating(POTION, 80).unwrap();
        assert_eq!(leftover, 0);
        assert_eq!(touched.len(), 8);
        let (touched2, leftover2) = inv.add_item_locating(POTION, 5).unwrap();
        assert_eq!(leftover2, 5, "no slot can accept more potions");
        assert!(touched2.is_empty(), "no slots touched on full reject");
    }

    // Track 14.2 — recompute helper tests. Build a minimal
    // PerConnection via from_spawn + a synthetic CharacterSpawn so
    // we can exercise the stat math against the real registry.
    fn make_conn() -> crate::world::connection::PerConnection {
        use crate::db::CharacterSpawn;
        use std::time::Instant;
        let spawn = CharacterSpawn {
            char_id: 1, account_id: 1,
            name: "Test".into(), race: "Human".into(), class: "Warrior".into(),
            level: 1, xp: 0, xp_to_next: 100,
            strength: 10, dexterity: 10, agility: 10, intelligence: 10,
            wisdom: 10, charisma: 10, constitution: 10,
            max_hp: 100.0, max_mp: 100.0, max_stamina: 100.0,
            hp: 100.0, mp: 100.0, stamina: 100.0,
            coins: 0, zone: None,
            pos: (0.0, 0.0, 0.0), yaw: 0.0,
        };
        crate::world::connection::PerConnection::from_spawn(spawn, Instant::now())
    }

    #[test]
    fn recompute_no_equipment_is_noop() {
        let mut conn = make_conn();
        let r = recompute_equipped_stats(&mut conn);
        assert!(!r.any_resource_max_changed());
        assert_eq!(conn.max_hp, 100.0);
        assert_eq!(conn.equipped_armor, 0);
        assert_eq!(conn.strength, 10);
    }

    #[test]
    fn recompute_picks_up_chest_armor_and_stats() {
        // Iron Chain Vest: armor=18, str_bonus=1, con_bonus=2,
        // max_hp_bonus=25.0 (see items.toml).
        let mut conn = make_conn();
        conn.inventory.equipment.insert(
            3,
            InventoryEntry {
                item_path: "res://data/loot/items/iron_chain_vest.tres".into(),
                count: 1,
            },
        );
        let r = recompute_equipped_stats(&mut conn);
        assert_eq!(conn.equipped_armor, 18);
        assert_eq!(conn.strength, 11, "+1 str from vest");
        assert_eq!(conn.constitution, 12, "+2 con from vest");
        assert_eq!(conn.max_hp, 125.0, "+25 max_hp from vest");
        assert!(r.max_hp_changed);
        // Current HP should NOT bump on equip — mirrors GDScript's
        // apply_item_bonuses which only touches max_*.
        assert_eq!(conn.hp, 100.0);
    }

    #[test]
    fn recompute_unequip_reverses_stats_and_clamps_current_hp() {
        let mut conn = make_conn();
        // First: equip the vest.
        conn.inventory.equipment.insert(
            3,
            InventoryEntry {
                item_path: "res://data/loot/items/iron_chain_vest.tres".into(),
                count: 1,
            },
        );
        recompute_equipped_stats(&mut conn);
        // Simulate the player healing into the bonus headroom.
        conn.hp = 125.0;
        assert_eq!(conn.max_hp, 125.0);
        // Now: unequip — empty the paperdoll slot.
        conn.inventory.equipment.remove(&3);
        let r = recompute_equipped_stats(&mut conn);
        assert_eq!(conn.max_hp, 100.0, "max_hp returns to base");
        assert_eq!(conn.hp, 100.0, "current hp clamped down to new max");
        assert_eq!(conn.equipped_armor, 0);
        assert_eq!(conn.strength, 10, "str back to base");
        assert_eq!(conn.constitution, 10, "con back to base");
        assert!(r.max_hp_changed);
    }

    #[test]
    fn recompute_unequip_below_current_does_not_drop_hp() {
        // Player wears vest (max 125), is at hp 80. Unequip drops
        // max to 100; current 80 < 100 so it stays put.
        let mut conn = make_conn();
        conn.inventory.equipment.insert(
            3,
            InventoryEntry {
                item_path: "res://data/loot/items/iron_chain_vest.tres".into(),
                count: 1,
            },
        );
        recompute_equipped_stats(&mut conn);
        conn.hp = 80.0;
        conn.inventory.equipment.remove(&3);
        recompute_equipped_stats(&mut conn);
        assert_eq!(conn.hp, 80.0, "hp unchanged when below new max");
        assert_eq!(conn.max_hp, 100.0);
    }

    #[test]
    fn recompute_sums_across_multiple_pieces() {
        // Vest (chest) + iron short sword (weapon) + cloth robe — wait,
        // can't have two chests. Use vest + sword.
        // Iron Short Sword: str_bonus=2 (per items.toml).
        let mut conn = make_conn();
        conn.inventory.equipment.insert(
            0,
            InventoryEntry {
                item_path: "res://data/loot/items/iron_short_sword.tres".into(),
                count: 1,
            },
        );
        conn.inventory.equipment.insert(
            3,
            InventoryEntry {
                item_path: "res://data/loot/items/iron_chain_vest.tres".into(),
                count: 1,
            },
        );
        recompute_equipped_stats(&mut conn);
        // STR: base 10 + vest 1 + sword 2 = 13.
        assert_eq!(conn.strength, 13);
        // Armor: only the vest contributes (sword has no armor field).
        assert_eq!(conn.equipped_armor, 18);
    }

    #[test]
    fn recompute_ignores_unknown_item_paths() {
        // An item path that isn't in the registry contributes
        // nothing (and doesn't blow up). Equipment that survived
        // a registry rename without re-export effectively becomes
        // a no-op until the path is re-added.
        let mut conn = make_conn();
        conn.inventory.equipment.insert(
            3,
            InventoryEntry {
                item_path: "res://nonexistent_chest.tres".into(),
                count: 1,
            },
        );
        let r = recompute_equipped_stats(&mut conn);
        assert!(!r.any_resource_max_changed());
        assert_eq!(conn.max_hp, 100.0);
        assert_eq!(conn.equipped_armor, 0);
    }

    #[test]
    fn recompute_orthogonal_to_external_max_hp_changes() {
        // Simulate a Bless buff adding +30 max_hp via the buffs
        // pathway (touches conn.max_hp directly without changing
        // equip_stat_bonuses). A subsequent equip recompute should
        // preserve the bless contribution and only add the gear
        // delta on top — and a later unequip should leave the
        // bless contribution intact.
        let mut conn = make_conn();
        conn.max_hp += 30.0; // bless applied externally.
        assert_eq!(conn.max_hp, 130.0);
        // Equip vest: +25 max_hp from gear, total should be 155.
        conn.inventory.equipment.insert(
            3,
            InventoryEntry {
                item_path: "res://data/loot/items/iron_chain_vest.tres".into(),
                count: 1,
            },
        );
        recompute_equipped_stats(&mut conn);
        assert_eq!(conn.max_hp, 155.0, "bless 30 + gear 25 on top of base 100");
        // Unequip vest: should drop by 25, back to 130 (bless intact).
        conn.inventory.equipment.remove(&3);
        recompute_equipped_stats(&mut conn);
        assert_eq!(conn.max_hp, 130.0, "gear undone; bless preserved");
    }

    #[test]
    fn add_item_partial_fill_returns_leftover() {
        // 7 stacks of 10 potions + slot 7 holds a sword. Adding 25
        // more potions should top up nothing, fail to claim slot 7
        // (occupied by sword), leave 25 - 10 = 15 leftover? Actually
        // no: pass 1 finds no same-item slot under cap (slots 0..7
        // are all at 10), pass 2 finds slot 7 occupied, no empties
        // → all 25 are leftover, 0 touched.
        let mut inv = PlayerInventory::new();
        for i in 0..7 {
            inv.base[i] = Some(InventoryEntry {
                item_path: POTION.into(),
                count: 10,
            });
        }
        inv.base[7] = Some(InventoryEntry { item_path: SWORD.into(), count: 1 });
        let (touched, leftover) = inv.add_item_locating(POTION, 25).unwrap();
        assert!(touched.is_empty());
        assert_eq!(leftover, 25);
    }

    #[test]
    fn unequip_moves_to_base() {
        let mut inv = PlayerInventory::new();
        inv.equipment.insert(0, InventoryEntry { item_path: "res://items/sword.tres".into(), count: 1 });
        let touched = inv.unequip_to_base(0, 3).expect("unequip");
        assert_eq!(touched, vec![("base", 3u32), ("equip", 0u32)]);
        assert!(inv.equipment.get(&0).is_none());
        assert_eq!(inv.base[3].as_ref().unwrap().item_path, "res://items/sword.tres");
    }

    #[test]
    fn unequip_swaps_with_existing_base_slot() {
        let mut inv = PlayerInventory::new();
        inv.equipment.insert(0, InventoryEntry { item_path: "res://items/sword.tres".into(), count: 1 });
        inv.add_item("res://items/cloth.tres", 5).unwrap();
        // Unequip into slot 0 which holds cloth: cloth ends up
        // equipped in slot 0 of paperdoll (no item-vs-slot check).
        inv.unequip_to_base(0, 0).expect("unequip-swap");
        assert_eq!(inv.base[0].as_ref().unwrap().item_path, "res://items/sword.tres");
        assert_eq!(inv.equipment.get(&0).unwrap().item_path, "res://items/cloth.tres");
    }

    #[test]
    fn unequip_rejects_empty_paperdoll_slot() {
        let mut inv = PlayerInventory::new();
        let err = inv.unequip_to_base(0, 0);
        assert!(err.is_err());
    }
}
