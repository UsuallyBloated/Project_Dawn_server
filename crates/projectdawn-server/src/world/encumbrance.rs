//! Encumbrance: carry weight against a STR-driven capacity, and what being
//! over it costs (slower movement, slower or no stamina regen). The server
//! owns it, because movement speed and regen are the server's to decide: the
//! client's `autoloads/encumbrance.gd` only predicts, with the same constants
//! and the same arithmetic, so its local movement and the server's agree.
//! Weight comes from coins (every coin weighs the same regardless of tier, so
//! a copper hoard is heavy; docs/concepts/world/currency.md) and from every
//! carried or worn stack, each item's `weight` (items.toml) times its count.
//!
//! Kept in lockstep by hand with `autoloads/encumbrance.gd`: change a constant
//! on one side and change it on the other.

use super::connection::PerConnection;

/// Weight of one coin of any tier.
pub const COIN_WEIGHT: f32 = 0.02;
/// capacity = BASE_CAPACITY + STR x CAPACITY_PER_STR. A STR-10 character
/// carries 20 (a thousand coins) before slowing.
pub const BASE_CAPACITY: f32 = 10.0;
pub const CAPACITY_PER_STR: f32 = 1.0;
/// Over capacity, speed falls by this much per 1.0 of weight/capacity ratio
/// above 1.0, never below MIN_SPEED_MULT (a crawl, never a full stop).
pub const SPEED_PENALTY_RATE: f32 = 0.6;
pub const MIN_SPEED_MULT: f32 = 0.25;
/// Stamina regen is halved while encumbered and stops at OVERLOADED_RATIO
/// times capacity.
pub const ENCUMBERED_ST_REGEN_MULT: f32 = 0.5;
pub const OVERLOADED_RATIO: f32 = 2.0;

/// Everything the character carries: coins, worn gear, the base slots, every
/// bag's contents (a held bag's too) and the cursor. An unknown item path
/// weighs nothing (it cannot be in an inventory through any honest path).
pub fn carried_weight(conn: &PerConnection) -> f32 {
    let c = &conn.coins;
    let coin_count = (c.platinum + c.gold + c.silver + c.copper).max(0) as f32;
    let mut w = coin_count * COIN_WEIGHT;
    for (path, count) in conn.inventory.all_stacks() {
        if let Some(item) = super::items::lookup(&path) {
            w += item.weight * count as f32;
        }
    }
    w
}

pub fn capacity(strength: i32) -> f32 {
    BASE_CAPACITY + strength as f32 * CAPACITY_PER_STR
}

/// The movement-speed multiplier for a weight against a capacity.
pub fn speed_mult(weight: f32, capacity: f32) -> f32 {
    if capacity <= 0.0 || weight <= capacity {
        return 1.0;
    }
    let ratio = weight / capacity;
    (1.0 - SPEED_PENALTY_RATE * (ratio - 1.0)).clamp(MIN_SPEED_MULT, 1.0)
}

/// The stamina-regen multiplier for a weight against a capacity.
pub fn stamina_regen_mult(weight: f32, capacity: f32) -> f32 {
    if capacity <= 0.0 || weight <= capacity {
        return 1.0;
    }
    if weight >= capacity * OVERLOADED_RATIO {
        return 0.0;
    }
    ENCUMBERED_ST_REGEN_MULT
}

/// Both multipliers for a connection as it stands now.
pub fn for_conn(conn: &PerConnection) -> (f32, f32) {
    let w = carried_weight(conn);
    let cap = capacity(conn.strength);
    (speed_mult(w, cap), stamina_regen_mult(w, cap))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_follows_strength() {
        assert_eq!(capacity(10), 20.0);
        assert_eq!(capacity(0), 10.0);
    }

    #[test]
    fn speed_is_unpenalised_up_to_capacity_then_falls_to_a_crawl() {
        assert_eq!(speed_mult(20.0, 20.0), 1.0);
        // 1.5x capacity: 1 - 0.6 x 0.5.
        assert!((speed_mult(30.0, 20.0) - 0.7).abs() < 1e-6);
        // Far over: floored, never a stop.
        assert_eq!(speed_mult(500.0, 20.0), MIN_SPEED_MULT);
    }

    #[test]
    fn stamina_regen_halves_when_encumbered_and_stops_when_overloaded() {
        assert_eq!(stamina_regen_mult(20.0, 20.0), 1.0);
        assert_eq!(stamina_regen_mult(30.0, 20.0), 0.5);
        assert_eq!(stamina_regen_mult(40.0, 20.0), 0.0);
    }

    #[test]
    fn items_toml_carries_weights() {
        // A sample the movement test leans on: the client's .tres weights
        // reached the server (tools/check_item_lockstep.gd compares them all).
        let bread = super::super::items::lookup("res://data/loot/items/bread_loaf.tres")
            .expect("bread exists");
        assert!((bread.weight - 0.3).abs() < 1e-6, "bread weighs {}", bread.weight);
    }
}
