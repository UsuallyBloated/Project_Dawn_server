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
use std::collections::HashMap;

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

    /// Track 13.2 — record the slot index used by the next `add_item`
    /// (stack target or first-empty), then mutate. Returns the slot
    /// index so the caller can fan a single targeted `InventoryDelta`
    /// rather than diffing before/after snapshots. None on full
    /// inventory.
    pub fn add_item_locating(
        &mut self,
        item_path: &str,
        count: u32,
    ) -> Result<usize, &'static str> {
        if count == 0 {
            return Err("zero count");
        }
        if item_path.is_empty() {
            return Err("empty item_path");
        }
        for (i, slot) in self.base.iter_mut().enumerate() {
            if let Some(entry) = slot {
                if entry.item_path == item_path {
                    entry.count = entry.count.saturating_add(count);
                    return Ok(i);
                }
            }
        }
        for (i, slot) in self.base.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(InventoryEntry {
                    item_path: item_path.to_string(),
                    count,
                });
                return Ok(i);
            }
        }
        Err("inventory full")
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

    /// Track 13.3 — equip the entry at base slot `src` into
    /// paperdoll slot `equip_slot`. If the paperdoll already holds
    /// an item, swap it back into the source slot. Returns the
    /// list of (location, slot) tuples touched so the caller fans
    /// one `InventoryDelta` per slot.
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
        let Some(src_entry) = self.base[src].take() else {
            return Err("source slot empty");
        };
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

    /// Add `count` of `item_path` to the inventory. Stacks onto an
    /// existing slot with the same item_path first (unbounded stacks
    /// for now — the client's max_stack enforces visual splitting
    /// while the server tracks total ownership), falls back to the
    /// first empty slot. Returns `Err` if the inventory is full.
    ///
    /// Track 13.2 prefers `add_item_locating` which returns the
    /// chosen slot index; this signature stays for the unit tests
    /// that just need to assert "did the item land somewhere."
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn add_item(&mut self, item_path: &str, count: u32) -> Result<(), &'static str> {
        if count == 0 {
            return Ok(());
        }
        self.add_item_locating(item_path, count).map(|_| ())
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

    #[test]
    fn equip_moves_from_base_to_paperdoll() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/sword.tres", 1).unwrap();
        let touched = inv.equip_from_base(0, 0).expect("equip");
        assert_eq!(touched, vec![("base", 0u32), ("equip", 0u32)]);
        assert!(inv.base[0].is_none());
        assert_eq!(inv.equipment.get(&0).unwrap().item_path, "res://items/sword.tres");
    }

    #[test]
    fn equip_swaps_with_existing_equipped() {
        let mut inv = PlayerInventory::new();
        inv.equipment.insert(0, InventoryEntry { item_path: "res://items/old_sword.tres".into(), count: 1 });
        inv.add_item("res://items/new_sword.tres", 1).unwrap();
        inv.equip_from_base(0, 0).expect("equip-swap");
        assert_eq!(inv.equipment.get(&0).unwrap().item_path, "res://items/new_sword.tres");
        assert_eq!(inv.base[0].as_ref().unwrap().item_path, "res://items/old_sword.tres", "old item returns to src slot");
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
        inv.add_item("res://items/sword.tres", 1).unwrap();
        assert!(inv.equip_from_base(0, EQUIP_SLOT_COUNT).is_err());
        assert!(inv.equip_from_base(BASE_SLOT_COUNT, 0).is_err());
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
