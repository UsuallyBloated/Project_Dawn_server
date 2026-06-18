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

/// Track 14.3 — parse a `bag_<i>` location string to its base-slot
/// index. Returns `None` for non-bag locations (`"base"`, `"equip"`)
/// or malformed / out-of-range bag indices.
fn parse_bag_location(loc: &str) -> Option<u8> {
    let suffix = loc.strip_prefix("bag_")?;
    let idx: u32 = suffix.parse().ok()?;
    if idx >= BASE_SLOT_COUNT as u32 {
        return None;
    }
    Some(idx as u8)
}

/// Track 14.3 — bag inner slots cannot hold a bag-typed item.
/// Mirrors the GDScript "no bag-in-bag" rule (`Inventory.add_item`
/// only places bags in base slots).
fn is_bag_item(path: &str) -> bool {
    items::bag_num_slots(path).is_some()
}

/// Track 14.3 — typed view of a wire `(location, slot)` pair.
enum SlotRefInt {
    Base(usize),
    Bag(u8, usize),
}

fn parse_slot_ref(loc: &str, slot: u32) -> Result<SlotRefInt, &'static str> {
    if loc == "base" {
        let idx = slot as usize;
        if idx >= BASE_SLOT_COUNT {
            return Err("base slot out of range");
        }
        return Ok(SlotRefInt::Base(idx));
    }
    if let Some(base_idx) = parse_bag_location(loc) {
        return Ok(SlotRefInt::Bag(base_idx, slot as usize));
    }
    Err("unsupported location")
}

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
    /// only occupied slots have entries. Track 14.1 added the
    /// item-vs-slot validation enforced inside `equip_from_base`.
    pub equipment: HashMap<u8, InventoryEntry>,
    /// Track 14.3 — per-bag contents. Sparse map keyed by the
    /// base-slot index that holds the bag-typed item. Each Vec is
    /// sized at the bag's `bag_num_slots` (from the registry) at
    /// the moment the bag was placed; entries are `None` for empty
    /// slots. Removing the bag from `base` requires the
    /// corresponding Vec to be all-`None`; rejection happens in
    /// `move_across` / `move_base` before any mutation lands.
    pub bags: HashMap<u8, Vec<Option<InventoryEntry>>>,
}

impl Default for PlayerInventory {
    fn default() -> Self {
        Self {
            base: vec![None; BASE_SLOT_COUNT],
            equipment: HashMap::new(),
            bags: HashMap::new(),
        }
    }
}

impl PlayerInventory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconstruct from DB rows. Handles 'base' (Track 13.1),
    /// 'equip' (Track 13.3), and 'bag_<i>' (Track 14.3). Two-pass:
    /// base + equip first, then bag_<i> rows so each bag's parent
    /// is in place (and its capacity known via the registry's
    /// `bag_num_slots`) before its contents land. Out-of-range
    /// slots and rows for a non-existent / non-bag parent are
    /// silently dropped.
    pub fn from_rows(rows: &[InventoryRow]) -> Self {
        let mut inv = Self::default();
        // Pass 1 — base + equip; initialise bag Vecs for any
        // bag-typed base entries so pass 2's bag_<i> rows can land.
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
                    inv.ensure_bag_init(idx);
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
                _ => {} // bag_<i> rows handled below
            }
        }
        // Pass 2 — bag_<i> rows. Routes into the Vec initialised
        // by pass 1's `ensure_bag_init`. Out-of-range bag slots
        // (or rows pointing at a base slot that turned out not to
        // hold a bag) drop silently.
        for row in rows {
            if row.count <= 0 || row.item_path.is_empty() {
                continue;
            }
            let Some(base_idx) = parse_bag_location(&row.location) else {
                continue;
            };
            let Some(arr) = inv.bags.get_mut(&base_idx) else {
                continue;
            };
            let slot_idx = row.slot as usize;
            if row.slot < 0 || slot_idx >= arr.len() {
                continue;
            }
            arr[slot_idx] = Some(InventoryEntry {
                item_path: row.item_path.clone(),
                count: row.count as u32,
            });
        }
        inv
    }

    /// Project into DB rows for persistence. Only emits filled slots.
    /// Emits 'base' + 'equip' + 'bag_<i>' rows.
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
        for (base_idx, arr) in self.bags.iter() {
            for (slot_idx, slot) in arr.iter().enumerate() {
                if let Some(entry) = slot {
                    out.push(InventoryRow {
                        location: format!("bag_{base_idx}"),
                        slot: slot_idx as i32,
                        item_path: entry.item_path.clone(),
                        count: entry.count as i32,
                    });
                }
            }
        }
        out
    }

    /// Track 13.2 / 14.3 — project to the wire shape used by
    /// `ServerWorldMsg::InventorySnapshot`. Parallel to `to_rows`
    /// but emits the protocol's (location, slot, item_path, count)
    /// tuple. Includes equip + bag_<i> entries.
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
        for (base_idx, arr) in self.bags.iter() {
            for (slot_idx, slot) in arr.iter().enumerate() {
                if let Some(entry) = slot {
                    out.push((
                        format!("bag_{base_idx}"),
                        slot_idx as u32,
                        entry.item_path.clone(),
                        entry.count,
                    ));
                }
            }
        }
        out
    }

    /// Track 14.3 — sync `bags[base_idx]` against whatever sits in
    /// `base[base_idx]`. Called after every mutation that could
    /// change a base entry. If the base slot now holds a bag-typed
    /// item, allocate an empty Vec of the registry's
    /// `bag_num_slots` (or leave the existing Vec untouched — see
    /// the empty-required move rule). If it no longer holds a bag,
    /// drop the Vec.
    pub fn ensure_bag_init(&mut self, base_idx: usize) {
        let key = base_idx as u8;
        let Some(entry) = self.base.get(base_idx).and_then(|s| s.as_ref()) else {
            self.bags.remove(&key);
            return;
        };
        match items::bag_num_slots(&entry.item_path) {
            Some(n) => {
                self.bags
                    .entry(key)
                    .or_insert_with(|| vec![None; n as usize]);
            }
            None => {
                self.bags.remove(&key);
            }
        }
    }

    /// Returns `true` when `base[idx]` holds a bag and the bag has
    /// at least one occupied inner slot. Used by `move_base` /
    /// `move_across` to enforce the "empty bag required to move
    /// out" rule.
    fn bag_at_base_is_nonempty(&self, base_idx: usize) -> bool {
        let key = base_idx as u8;
        match self.bags.get(&key) {
            Some(arr) => arr.iter().any(|s| s.is_some()),
            None => false,
        }
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
                // Track 14.3 — bag-typed loot needs its inner Vec
                // allocated so subsequent bag_<i> deltas have
                // somewhere to land.
                self.ensure_bag_init(i);
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
        // Track 14.3 — split can't change item type at either slot,
        // but ensure_bag_init keeps the bags map consistent if the
        // src stack went to zero (clears the bags entry that
        // shouldn't exist for a non-bag item anyway) and is cheap
        // to call defensively.
        self.ensure_bag_init(src);
        self.ensure_bag_init(dst);
        Ok(vec![src, dst])
    }

    /// Track 13.3 / 14.3 — unequip paperdoll slot `equip_slot`
    /// into base slot `dst`. If dst is occupied, the swap pushes
    /// the displaced base item into the paperdoll — which means
    /// it has to be equippable in that slot, OR the swap rejects
    /// (Track 14.3 hardening: stops bags or consumables from
    /// being pushed into the paperdoll via a swap unequip).
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
        // Track 14.3 — if the swap would land an unsuitable item
        // (bag, consumable, mismatched gear type) in the paperdoll,
        // reject before any mutation.
        if let Some(existing) = self.base.get(dst).and_then(|s| s.as_ref()) {
            if !items::is_equippable_in_slot(&existing.item_path, equip_slot) {
                return Err("base item not equippable in this slot");
            }
        }
        let Some(equip_entry) = self.equipment.remove(&equip_slot) else {
            return Err("equip slot empty");
        };
        let prev_base = self.base[dst].take();
        self.base[dst] = Some(equip_entry);
        if let Some(prev) = prev_base {
            self.equipment.insert(equip_slot, prev);
        }
        self.ensure_bag_init(dst);
        Ok(vec![("base", dst as u32), ("equip", equip_slot as u32)])
    }

    /// Track 15.1 — equip from any inventory location (base or bag
    /// inner). Generalisation of `equip_from_base`: the wire's
    /// `(src_location, src_slot)` can now address bag inner slots so
    /// the client doesn't need a base-bridge hop. Item-type validation
    /// reuses `items::is_equippable_in_slot`; swap pushes the
    /// previously-equipped item back into the same source location.
    pub fn equip_from_location(
        &mut self,
        src_loc: &str,
        src_slot: u32,
        equip_slot: u8,
    ) -> Result<Vec<(String, u32)>, &'static str> {
        if equip_slot >= EQUIP_SLOT_COUNT {
            return Err("equip slot out of range");
        }
        let src = parse_slot_ref(src_loc, src_slot)?;
        // Peek the src item to validate equippability before any mutation.
        let src_path = match src {
            SlotRefInt::Base(s) => match self.base.get(s).and_then(|e| e.as_ref()) {
                Some(e) => e.item_path.clone(),
                None => return Err("source slot empty"),
            },
            SlotRefInt::Bag(b, s) => {
                let arr = self.bags.get(&b).ok_or("source bag does not exist")?;
                if s >= arr.len() {
                    return Err("bag slot out of range");
                }
                match arr[s].as_ref() {
                    Some(e) => e.item_path.clone(),
                    None => return Err("source slot empty"),
                }
            }
        };
        if !items::is_equippable_in_slot(&src_path, equip_slot) {
            return Err("item not equippable in this slot");
        }
        // Take src, swap with paperdoll.
        let src_entry = match src {
            SlotRefInt::Base(s) => self.base[s].take().expect("checked above"),
            SlotRefInt::Bag(b, s) => self
                .bags
                .get_mut(&b)
                .expect("checked above")[s]
                .take()
                .expect("checked above"),
        };
        let prev_equip = self.equipment.remove(&equip_slot);
        self.equipment.insert(equip_slot, src_entry);
        if let Some(prev) = prev_equip {
            match src {
                SlotRefInt::Base(s) => self.base[s] = Some(prev),
                SlotRefInt::Bag(b, s) => {
                    self.bags.get_mut(&b).expect("checked above")[s] = Some(prev);
                }
            }
        }
        if let SlotRefInt::Base(s) = src {
            self.ensure_bag_init(s);
        }
        let touched_src = match src {
            SlotRefInt::Base(s) => ("base".to_string(), s as u32),
            SlotRefInt::Bag(b, s) => (format!("bag_{b}"), s as u32),
        };
        Ok(vec![touched_src, ("equip".to_string(), equip_slot as u32)])
    }

    /// Track 15.1 — destroy `count` of the entry at `(location, slot)`
    /// outright (no loot bag, no recovery). `count == 0` removes the
    /// whole stack. Rejects bag-typed slots that still hold items (a
    /// bag must be emptied first). Returns the removed `(item_path,
    /// count)` so the caller can log + fan a single `InventoryDelta`
    /// for the touched slot. Shared by the DestroyItem and DropItem
    /// apply paths (DropItem additionally spawns a loot bag).
    pub fn destroy_at(
        &mut self,
        loc: &str,
        slot: u32,
        count: u32,
    ) -> Result<(String, u32), &'static str> {
        let parsed = parse_slot_ref(loc, slot)?;
        match parsed {
            SlotRefInt::Base(s) => {
                if self.bag_at_base_is_nonempty(s) {
                    return Err("bag must be emptied before destroying");
                }
                let entry = self
                    .base
                    .get_mut(s)
                    .and_then(|e| e.as_mut())
                    .ok_or("source slot empty")?;
                let path = entry.item_path.clone();
                let to_remove = if count == 0 || count >= entry.count {
                    entry.count
                } else {
                    count
                };
                entry.count -= to_remove;
                if entry.count == 0 {
                    self.base[s] = None;
                }
                self.ensure_bag_init(s);
                Ok((path, to_remove))
            }
            SlotRefInt::Bag(b, s) => {
                let arr = self.bags.get_mut(&b).ok_or("source bag does not exist")?;
                if s >= arr.len() {
                    return Err("bag slot out of range");
                }
                let entry = arr[s].as_mut().ok_or("source slot empty")?;
                let path = entry.item_path.clone();
                let to_remove = if count == 0 || count >= entry.count {
                    entry.count
                } else {
                    count
                };
                entry.count -= to_remove;
                if entry.count == 0 {
                    arr[s] = None;
                }
                Ok((path, to_remove))
            }
        }
    }

    /// Track 15.2 — consume one unit from `(location, slot)` for the
    /// use-consumable flow. Caller is responsible for validating the
    /// item is actually a consumable (`heal_on_use`, `is_food`,
    /// `is_drink`) before calling — this just performs the
    /// inventory mutation. Returns the consumed `item_path` so the
    /// apply phase can look up the effect deltas.
    pub fn decrement_at(
        &mut self,
        loc: &str,
        slot: u32,
    ) -> Result<String, &'static str> {
        let parsed = parse_slot_ref(loc, slot)?;
        match parsed {
            SlotRefInt::Base(s) => {
                let entry = self
                    .base
                    .get_mut(s)
                    .and_then(|e| e.as_mut())
                    .ok_or("source slot empty")?;
                let path = entry.item_path.clone();
                entry.count -= 1;
                if entry.count == 0 {
                    self.base[s] = None;
                }
                self.ensure_bag_init(s);
                Ok(path)
            }
            SlotRefInt::Bag(b, s) => {
                let arr = self.bags.get_mut(&b).ok_or("source bag does not exist")?;
                if s >= arr.len() {
                    return Err("bag slot out of range");
                }
                let entry = arr[s].as_mut().ok_or("source slot empty")?;
                let path = entry.item_path.clone();
                entry.count -= 1;
                if entry.count == 0 {
                    arr[s] = None;
                }
                Ok(path)
            }
        }
    }

    /// Track 13.2 / 14.3 — atomic move/swap between base slots.
    /// Move-to-empty is a clean transfer; move-to-occupied with the
    /// same item_path merges counts (caps at u32::MAX); move-to-
    /// occupied with a different item_path is a swap.
    ///
    /// Track 14.3 — moving a bag in or out of a base slot requires
    /// the bag to be empty. A non-empty bag at `src` rejects the
    /// move (`bag must be emptied before moving`); a non-empty bag
    /// at `dst` (during a same-path merge attempt the bag is the
    /// src item, so this only triggers on swap) also rejects.
    /// `ensure_bag_init` is called for both indices after the
    /// mutation lands so the bags map stays consistent with the
    /// new base content.
    pub fn move_base(&mut self, src: usize, dst: usize) -> Result<Vec<usize>, &'static str> {
        if src >= BASE_SLOT_COUNT || dst >= BASE_SLOT_COUNT {
            return Err("slot out of range");
        }
        if src == dst {
            return Ok(Vec::new());
        }
        // Bag-empty validation runs before any take/insert so a
        // rejected move leaves the inventory untouched.
        if self.bag_at_base_is_nonempty(src) {
            return Err("bag must be emptied before moving");
        }
        if self.bag_at_base_is_nonempty(dst) {
            return Err("destination bag must be emptied before swapping");
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
        self.ensure_bag_init(src);
        self.ensure_bag_init(dst);
        Ok(vec![src, dst])
    }

    /// Track 14.3 — move/swap/merge between two inner slots of the
    /// same bag (`bag_<i>` → `bag_<i>`). Same semantics as
    /// `move_base` minus the bag-empty rule (we're inside the bag,
    /// not moving the bag itself). Returns the touched
    /// `(location, slot)` tuples for the InventoryDelta fan.
    pub fn move_bag(
        &mut self,
        base_idx: usize,
        src: usize,
        dst: usize,
    ) -> Result<Vec<(String, u32)>, &'static str> {
        let key = base_idx as u8;
        let arr = self
            .bags
            .get_mut(&key)
            .ok_or("bag does not exist at base index")?;
        if src >= arr.len() || dst >= arr.len() {
            return Err("bag slot out of range");
        }
        if src == dst {
            return Ok(Vec::new());
        }
        let src_entry = arr[src].take();
        let Some(src_entry) = src_entry else {
            return Err("source slot empty");
        };
        let dst_entry = arr[dst].take();
        match dst_entry {
            None => {
                arr[dst] = Some(src_entry);
            }
            Some(existing) if existing.item_path == src_entry.item_path => {
                let merged_count = existing.count.saturating_add(src_entry.count);
                arr[dst] = Some(InventoryEntry {
                    item_path: existing.item_path,
                    count: merged_count,
                });
            }
            Some(existing) => {
                arr[src] = Some(existing);
                arr[dst] = Some(src_entry);
            }
        }
        let loc = format!("bag_{base_idx}");
        Ok(vec![(loc.clone(), src as u32), (loc, dst as u32)])
    }

    /// Track 14.3 — top-level move dispatch. Resolves the
    /// `(location, slot)` pair on each side and routes to the
    /// appropriate inner helper. Supports every combination of
    /// `"base"` and `"bag_<i>"`; `"equip"` locations stay on the
    /// dedicated `equip_from_base` / `unequip_to_base` paths.
    pub fn move_across(
        &mut self,
        src_loc: &str,
        src_slot: u32,
        dst_loc: &str,
        dst_slot: u32,
    ) -> Result<Vec<(String, u32)>, &'static str> {
        let src = parse_slot_ref(src_loc, src_slot)?;
        let dst = parse_slot_ref(dst_loc, dst_slot)?;
        match (src, dst) {
            (SlotRefInt::Base(s), SlotRefInt::Base(d)) => {
                let touched = self.move_base(s, d)?;
                Ok(touched
                    .into_iter()
                    .map(|i| ("base".to_string(), i as u32))
                    .collect())
            }
            (SlotRefInt::Bag(b1, s), SlotRefInt::Bag(b2, d)) if b1 == b2 => {
                self.move_bag(b1 as usize, s, d)
            }
            (SlotRefInt::Bag(b1, s), SlotRefInt::Bag(b2, d)) => {
                self.move_bag_to_bag(b1, s, b2, d)
            }
            (SlotRefInt::Base(s), SlotRefInt::Bag(base_idx, d)) => {
                self.move_base_to_bag(s, base_idx, d)
            }
            (SlotRefInt::Bag(base_idx, s), SlotRefInt::Base(d)) => {
                self.move_bag_to_base(base_idx, s, d)
            }
        }
    }

    /// Track 14.3 — base → bag inner. Rejects bag-in-bag attempts
    /// (the src item must not itself be a bag). Same merge / swap
    /// semantics as the base ↔ base path; swap pushes the bag
    /// inner's old item back into the source base slot, which is
    /// safe (any item type can sit in base). `ensure_bag_init`
    /// runs against the source slot afterwards in case a bag was
    /// swapped out.
    fn move_base_to_bag(
        &mut self,
        src: usize,
        base_idx: u8,
        dst: usize,
    ) -> Result<Vec<(String, u32)>, &'static str> {
        if src >= BASE_SLOT_COUNT {
            return Err("base slot out of range");
        }
        let src_path = match self.base.get(src).and_then(|s| s.as_ref()) {
            Some(e) => e.item_path.clone(),
            None => return Err("source slot empty"),
        };
        if is_bag_item(&src_path) {
            return Err("cannot place a bag inside a bag");
        }
        let arr = self
            .bags
            .get_mut(&base_idx)
            .ok_or("destination bag does not exist")?;
        if dst >= arr.len() {
            return Err("bag slot out of range");
        }
        let src_entry = self.base[src].take().expect("checked above");
        let dst_entry = arr[dst].take();
        match dst_entry {
            None => {
                arr[dst] = Some(src_entry);
            }
            Some(existing) if existing.item_path == src_entry.item_path => {
                let merged_count = existing.count.saturating_add(src_entry.count);
                arr[dst] = Some(InventoryEntry {
                    item_path: existing.item_path,
                    count: merged_count,
                });
            }
            Some(existing) => {
                // Swap — bag inner's old item goes back to base[src].
                self.base[src] = Some(existing);
                arr[dst] = Some(src_entry);
            }
        }
        self.ensure_bag_init(src);
        Ok(vec![
            ("base".to_string(), src as u32),
            (format!("bag_{base_idx}"), dst as u32),
        ])
    }

    /// Track 14.3 — bag inner → base. Inner items are never bags
    /// (see `move_base_to_bag`), so the only ban here is when a
    /// swap would push a bag from `base[dst]` back into the bag
    /// inner slot. Empty-bag-required-to-move rule on dst applies
    /// only when swapping a bag out — empty bag swap would land
    /// the bag in the inner slot which is bag-in-bag and rejected.
    fn move_bag_to_base(
        &mut self,
        base_idx: u8,
        src: usize,
        dst: usize,
    ) -> Result<Vec<(String, u32)>, &'static str> {
        if dst >= BASE_SLOT_COUNT {
            return Err("base slot out of range");
        }
        let arr = self
            .bags
            .get_mut(&base_idx)
            .ok_or("source bag does not exist")?;
        if src >= arr.len() {
            return Err("bag slot out of range");
        }
        // Inspect the swap target before pulling from the bag so we
        // can reject cleanly.
        if let Some(existing) = self.base.get(dst).and_then(|s| s.as_ref()) {
            if is_bag_item(&existing.item_path) {
                return Err("cannot swap a bag into another bag's inner slot");
            }
        }
        let src_entry = arr[src].take().ok_or("source slot empty")?;
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
                // Swap — base[dst]'s item goes back to the bag inner.
                // (We've already verified it isn't a bag.)
                let arr = self
                    .bags
                    .get_mut(&base_idx)
                    .expect("bag existed at start of fn");
                arr[src] = Some(existing);
                self.base[dst] = Some(src_entry);
            }
        }
        self.ensure_bag_init(dst);
        Ok(vec![
            (format!("bag_{base_idx}"), src as u32),
            ("base".to_string(), dst as u32),
        ])
    }

    /// Track 14.3 — bag inner → different bag inner. Neither side
    /// can hold a bag (bag-in-bag ban applies to both source and
    /// destination on swap). Same merge / swap semantics as the
    /// same-bag path.
    fn move_bag_to_bag(
        &mut self,
        src_base: u8,
        src_slot: usize,
        dst_base: u8,
        dst_slot: usize,
    ) -> Result<Vec<(String, u32)>, &'static str> {
        // Validate both bags exist + slots are in range before any
        // mutation. Snapshot the src entry, peek the dst, then
        // commit.
        let src_entry = {
            let arr = self
                .bags
                .get_mut(&src_base)
                .ok_or("source bag does not exist")?;
            if src_slot >= arr.len() {
                return Err("source bag slot out of range");
            }
            arr[src_slot].take().ok_or("source slot empty")?
        };
        // Restore on any subsequent failure.
        let restore = |inv: &mut Self, entry: InventoryEntry| {
            if let Some(arr) = inv.bags.get_mut(&src_base) {
                arr[src_slot] = Some(entry);
            }
        };
        let dst_entry = {
            let arr = match self.bags.get_mut(&dst_base) {
                Some(a) => a,
                None => {
                    restore(self, src_entry);
                    return Err("destination bag does not exist");
                }
            };
            if dst_slot >= arr.len() {
                restore(self, src_entry);
                return Err("destination bag slot out of range");
            }
            arr[dst_slot].take()
        };
        match dst_entry {
            None => {
                self.bags.get_mut(&dst_base).expect("checked")[dst_slot] = Some(src_entry);
            }
            Some(existing) if existing.item_path == src_entry.item_path => {
                let merged_count = existing.count.saturating_add(src_entry.count);
                self.bags.get_mut(&dst_base).expect("checked")[dst_slot] = Some(InventoryEntry {
                    item_path: existing.item_path,
                    count: merged_count,
                });
            }
            Some(existing) => {
                self.bags.get_mut(&dst_base).expect("checked")[dst_slot] = Some(src_entry);
                self.bags.get_mut(&src_base).expect("checked")[src_slot] = Some(existing);
            }
        }
        Ok(vec![
            (format!("bag_{src_base}"), src_slot as u32),
            (format!("bag_{dst_base}"), dst_slot as u32),
        ])
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

    // `destroy_at` is the shared base/bag removal primitive behind both
    // the DestroyItem and DropItem apply paths; these cover its base-slot
    // arithmetic (the bag path is covered by destroy_at_rejects_non_empty_bag).
    #[test]
    fn destroy_at_base_partial_keeps_residual() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 10).unwrap();
        let (path, removed) = inv.destroy_at("base", 0, 3).expect("remove");
        assert_eq!(path, "res://items/cloth.tres");
        assert_eq!(removed, 3);
        assert_eq!(inv.base[0].as_ref().unwrap().count, 7);
    }

    #[test]
    fn destroy_at_base_zero_count_removes_whole_stack() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 10).unwrap();
        let (_, removed) = inv.destroy_at("base", 0, 0).expect("remove whole");
        assert_eq!(removed, 10);
        assert!(inv.base[0].is_none());
    }

    #[test]
    fn destroy_at_base_count_exceeds_stack_removes_all() {
        let mut inv = PlayerInventory::new();
        inv.add_item("res://items/cloth.tres", 5).unwrap();
        let (_, removed) = inv.destroy_at("base", 0, 99).expect("remove over-cap");
        assert_eq!(removed, 5, "removal caps at the stack size");
        assert!(inv.base[0].is_none());
    }

    #[test]
    fn destroy_at_base_empty_slot_errors() {
        let mut inv = PlayerInventory::new();
        assert!(inv.destroy_at("base", 0, 1).is_err());
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
        let touched = inv.equip_from_location("base", 0, 0).expect("equip");
        assert_eq!(touched, vec![("base".to_string(), 0u32), ("equip".to_string(), 0u32)]);
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
        inv.equip_from_location("base", 0, 0).expect("equip-swap");
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
        let err = inv.equip_from_location("base", 0, 0);
        assert!(err.is_err());
    }

    #[test]
    fn equip_rejects_out_of_range_slots() {
        let mut inv = PlayerInventory::new();
        inv.add_item(SWORD, 1).unwrap();
        assert!(inv.equip_from_location("base", 0, EQUIP_SLOT_COUNT).is_err());
        assert!(inv.equip_from_location("base", BASE_SLOT_COUNT as u32, 0).is_err());
    }

    #[test]
    fn equip_rejects_wrong_slot_type() {
        // Cloth robe (chest, slot 3) into weapon slot (0) must reject
        // without mutating either slot.
        let mut inv = PlayerInventory::new();
        inv.add_item(ROBE, 1).unwrap();
        let err = inv.equip_from_location("base", 0, 0);
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
            assert!(inv.equip_from_location("base", 0, slot).is_err());
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
        assert!(inv.equip_from_location("base", 0, 0).is_err());
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
            coins: protocol::world::Coins::ZERO, bank_coins: protocol::world::Coins::ZERO, zone: None,
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
        // Both items must be equippable in the target slot for the
        // swap to land; Track 14.3 added the dst-equippable check.
        let mut inv = PlayerInventory::new();
        inv.equipment.insert(
            0,
            InventoryEntry { item_path: SWORD.into(), count: 1 },
        );
        inv.add_item("res://data/loot/items/iron_dagger.tres", 1).unwrap();
        inv.unequip_to_base(0, 0).expect("unequip-swap");
        assert_eq!(inv.base[0].as_ref().unwrap().item_path, SWORD);
        assert_eq!(
            inv.equipment.get(&0).unwrap().item_path,
            "res://data/loot/items/iron_dagger.tres"
        );
    }

    #[test]
    fn unequip_rejects_swap_into_nonequippable_base() {
        // Potion can't be equipped, so unequip-into-base-with-potion
        // must reject without mutating either slot.
        let mut inv = PlayerInventory::new();
        inv.equipment.insert(
            0,
            InventoryEntry { item_path: SWORD.into(), count: 1 },
        );
        inv.add_item(POTION, 1).unwrap();
        let err = inv.unequip_to_base(0, 0);
        assert!(err.is_err());
        assert_eq!(
            inv.equipment.get(&0).unwrap().item_path,
            SWORD,
            "equip unchanged on reject"
        );
        assert_eq!(
            inv.base[0].as_ref().unwrap().item_path,
            POTION,
            "base unchanged on reject"
        );
    }

    #[test]
    fn unequip_rejects_empty_paperdoll_slot() {
        let mut inv = PlayerInventory::new();
        let err = inv.unequip_to_base(0, 0);
        assert!(err.is_err());
    }

    // Track 14.3 — bag location tests.
    const POUCH: &str = "res://data/loot/items/small_pouch.tres";

    #[test]
    fn ensure_bag_init_allocates_inner_vec_for_bag() {
        let mut inv = PlayerInventory::new();
        inv.base[3] = Some(InventoryEntry {
            item_path: POUCH.into(),
            count: 1,
        });
        inv.ensure_bag_init(3);
        let arr = inv.bags.get(&3u8).expect("bag init");
        assert_eq!(arr.len(), 4, "small_pouch has bag_num_slots=4");
        assert!(arr.iter().all(|s| s.is_none()));
    }

    #[test]
    fn ensure_bag_init_clears_when_base_no_longer_bag() {
        let mut inv = PlayerInventory::new();
        inv.base[3] = Some(InventoryEntry {
            item_path: POUCH.into(),
            count: 1,
        });
        inv.ensure_bag_init(3);
        assert!(inv.bags.contains_key(&3u8));
        inv.base[3] = Some(InventoryEntry {
            item_path: SWORD.into(),
            count: 1,
        });
        inv.ensure_bag_init(3);
        assert!(!inv.bags.contains_key(&3u8));
    }

    #[test]
    fn add_item_locating_initialises_bag_on_loot_grant() {
        let mut inv = PlayerInventory::new();
        let (touched, leftover) = inv.add_item_locating(POUCH, 1).unwrap();
        assert_eq!(leftover, 0);
        assert_eq!(touched, vec![0]);
        assert!(
            inv.bags.contains_key(&0u8),
            "looted bag must allocate its inner Vec"
        );
    }

    #[test]
    fn move_bag_swaps_inner_slots() {
        let mut inv = PlayerInventory::new();
        inv.add_item_locating(POUCH, 1).unwrap();
        let bag = inv.bags.get_mut(&0u8).unwrap();
        bag[0] = Some(InventoryEntry {
            item_path: POTION.into(),
            count: 3,
        });
        bag[2] = Some(InventoryEntry {
            item_path: SWORD.into(),
            count: 1,
        });
        let touched = inv.move_bag(0, 0, 2).expect("swap");
        assert_eq!(touched.len(), 2);
        let bag = inv.bags.get(&0u8).unwrap();
        assert_eq!(bag[0].as_ref().unwrap().item_path, SWORD);
        assert_eq!(bag[2].as_ref().unwrap().item_path, POTION);
    }

    #[test]
    fn move_base_rejects_non_empty_bag() {
        let mut inv = PlayerInventory::new();
        inv.base[0] = Some(InventoryEntry {
            item_path: POUCH.into(),
            count: 1,
        });
        inv.ensure_bag_init(0);
        inv.bags.get_mut(&0u8).unwrap()[0] = Some(InventoryEntry {
            item_path: POTION.into(),
            count: 2,
        });
        let err = inv.move_base(0, 4);
        assert!(err.is_err(), "non-empty bag must reject move");
        assert!(inv.base[4].is_none(), "destination untouched");
        assert!(inv.base[0].is_some(), "source untouched");
    }

    #[test]
    fn move_base_allows_empty_bag() {
        let mut inv = PlayerInventory::new();
        inv.base[0] = Some(InventoryEntry {
            item_path: POUCH.into(),
            count: 1,
        });
        inv.ensure_bag_init(0);
        inv.move_base(0, 4).expect("empty bag moves");
        assert_eq!(inv.base[4].as_ref().unwrap().item_path, POUCH);
        assert!(inv.base[0].is_none());
        assert!(
            inv.bags.contains_key(&4u8),
            "bag entry follows the bag to its new slot"
        );
        assert!(
            !inv.bags.contains_key(&0u8),
            "bag entry leaves the old slot"
        );
    }

    #[test]
    fn move_across_base_to_bag_rejects_bag_in_bag() {
        // Putting a bag-typed item inside another bag's inner slot
        // must reject — no bag-in-bag.
        let mut inv = PlayerInventory::new();
        inv.add_item_locating(POUCH, 1).unwrap(); // base[0] = pouch
        inv.base[1] = Some(InventoryEntry {
            item_path: POUCH.into(),
            count: 1,
        });
        inv.ensure_bag_init(1);
        // Source: base[1] = a (different) pouch. Dst: bag at base 0,
        // inner slot 2.
        let err = inv.move_across("base", 1, "bag_0", 2);
        assert!(err.is_err());
    }

    #[test]
    fn move_across_base_to_bag_transfers_item() {
        let mut inv = PlayerInventory::new();
        inv.add_item_locating(POUCH, 1).unwrap(); // base[0] = pouch
        inv.base[1] = Some(InventoryEntry {
            item_path: POTION.into(),
            count: 5,
        });
        let touched = inv
            .move_across("base", 1, "bag_0", 2)
            .expect("transfer");
        assert_eq!(touched.len(), 2);
        assert!(inv.base[1].is_none(), "source cleared");
        assert_eq!(
            inv.bags.get(&0u8).unwrap()[2].as_ref().unwrap().item_path,
            POTION
        );
    }

    #[test]
    fn move_across_bag_to_base_swap_rejects_bag_dst() {
        // base[1] holds a different pouch (a bag). Trying to swap
        // a non-bag item out of bag 0 into base 1 would push the
        // bag from base 1 into bag 0's inner slot → bag-in-bag.
        let mut inv = PlayerInventory::new();
        inv.add_item_locating(POUCH, 1).unwrap(); // base[0] = pouch
        inv.bags.get_mut(&0u8).unwrap()[0] = Some(InventoryEntry {
            item_path: POTION.into(),
            count: 3,
        });
        inv.base[1] = Some(InventoryEntry {
            item_path: POUCH.into(),
            count: 1,
        });
        inv.ensure_bag_init(1);
        let err = inv.move_across("bag_0", 0, "base", 1);
        assert!(err.is_err());
    }

    #[test]
    fn destroy_at_rejects_non_empty_bag() {
        let mut inv = PlayerInventory::new();
        inv.add_item_locating(POUCH, 1).unwrap();
        inv.bags.get_mut(&0u8).unwrap()[0] = Some(InventoryEntry {
            item_path: POTION.into(),
            count: 1,
        });
        assert!(
            inv.destroy_at("base", 0, 0).is_err(),
            "removing a non-empty bag is rejected"
        );
        assert!(inv.base[0].is_some(), "bag still in base after rejected removal");
    }

    #[test]
    fn roundtrip_with_bag_contents() {
        let mut inv = PlayerInventory::new();
        inv.add_item_locating(POUCH, 1).unwrap();
        inv.bags.get_mut(&0u8).unwrap()[0] = Some(InventoryEntry {
            item_path: POTION.into(),
            count: 3,
        });
        inv.bags.get_mut(&0u8).unwrap()[3] = Some(InventoryEntry {
            item_path: SWORD.into(),
            count: 1,
        });
        let rows = inv.to_rows();
        // 1 base row (pouch) + 2 bag_0 rows.
        assert_eq!(rows.len(), 3, "{rows:?}");
        let restored = PlayerInventory::from_rows(&rows);
        assert_eq!(restored.base[0].as_ref().unwrap().item_path, POUCH);
        let bag = restored.bags.get(&0u8).expect("bag restored");
        assert_eq!(bag.len(), 4, "vec sized from registry's bag_num_slots");
        assert_eq!(bag[0].as_ref().unwrap().item_path, POTION);
        assert_eq!(bag[0].as_ref().unwrap().count, 3);
        assert_eq!(bag[3].as_ref().unwrap().item_path, SWORD);
        assert!(bag[1].is_none() && bag[2].is_none());
    }

    #[test]
    fn snapshot_entries_include_bag_rows() {
        let mut inv = PlayerInventory::new();
        inv.add_item_locating(POUCH, 1).unwrap();
        inv.bags.get_mut(&0u8).unwrap()[1] = Some(InventoryEntry {
            item_path: POTION.into(),
            count: 2,
        });
        let entries = inv.to_snapshot_entries();
        let has_pouch = entries
            .iter()
            .any(|(loc, slot, p, _)| loc == "base" && *slot == 0 && p == POUCH);
        let has_potion = entries.iter().any(|(loc, slot, p, c)| {
            loc == "bag_0" && *slot == 1 && p == POTION && *c == 2
        });
        assert!(has_pouch);
        assert!(has_potion);
    }
}
