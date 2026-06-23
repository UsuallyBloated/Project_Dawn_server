//! Server-authoritative XP + leveling (Slice 0 of the corpse / resurrection
//! epic, see docs/design/corpse_and_resurrection_plan.md).
//!
//! The server owns xp and level. Kill credit, quest turn-ins, and the death
//! penalty all flow through [`award_xp`], the single choke point that mutates
//! `conn.xp`, resolves any level up/down, and tells the client.
//!
//! Level changes apply the same per-level *intrinsic* stat deltas the GDScript
//! client's `_level_up` uses. The two tables (`char_data::level_gains` and the
//! client's `CLASS_LEVEL_GAINS`) are kept in lockstep and anchor-tested, so
//! moving authority here does not shift anyone's stats. Crucially we add only
//! the delta to `conn`'s running totals, never overwrite them: gear and buff
//! bonuses are already baked into those totals (see
//! `inventory::recompute_equipped_stats`), and overwriting would wipe them.
//!
//! The client mirrors: it renders the authoritative level + xp the server
//! sends (`LevelUp` / `XpGained`) and applies the deterministic stat deltas
//! locally. Max pools stay owned by the server's resource fan, so the client
//! never double-counts them.

use renet::{ClientId, RenetServer};

use crate::char_data;

use super::connection::PerConnection;
use super::handlers;

/// Death penalty parameters, locked with the user 2026-06-22:
/// a death costs 5% of the current level's XP band, the loss cascades past the
/// level boundary with no per-death cap, and levels 1 to 4 are exempt while a
/// cascade can never drop a character below level 5 (grace line == floor line).
pub const DEATH_XP_LOSS_FRACTION: f32 = 0.05;
pub const DEATH_PENALTY_FLOOR_LEVEL: i32 = 5;

/// Award `amount` xp (negative drains it) and resolve any level change.
///
/// Always sends the private `XpGained` feed with the authoritative current /
/// to-next totals. On a level change it also pushes `LevelUp`; the client
/// applies the matching intrinsic max-pool delta locally (the level tables are
/// in lockstep), so we don't fan the new maxes here. The regular `HealthUpdate`
/// remains the authoritative backstop for max pools (an idempotent assign).
/// Returns whether the level moved.
pub fn award_xp(server: &mut RenetServer, conn: &mut PerConnection, amount: i32) -> bool {
    let owner_cid = conn.char_id as ClientId;
    let old_level = conn.level;

    conn.xp += amount;
    let (new_xp, new_level, new_xp_to_next) = resolve(conn.xp, conn.level);
    conn.xp = new_xp;
    conn.level = new_level;
    conn.xp_to_next = new_xp_to_next;

    let leveled = conn.level != old_level;
    if leveled {
        apply_intrinsic_delta(conn, old_level, conn.level);
        handlers::send_level_up(server, owner_cid, conn.level as u32, conn.xp, conn.xp_to_next);
        tracing::info!(
            char_id = conn.char_id,
            from = old_level,
            to = conn.level,
            "level change",
        );
    }
    handlers::send_xp_gained(server, owner_cid, amount, conn.xp, conn.xp_to_next);
    leveled
}

/// Apply the death XP penalty: 5% of the current level's band, cascading down
/// with no per-death cap, but exempt below level 5 and floored at level 5.
/// Routes through [`award_xp`] so the de-level fans the same messages.
pub fn apply_death_penalty(server: &mut RenetServer, conn: &mut PerConnection) {
    if conn.level < DEATH_PENALTY_FLOOR_LEVEL {
        return; // grace: levels 1 to 4 lose no xp on death
    }
    let loss = (conn.xp_to_next as f32 * DEATH_XP_LOSS_FRACTION).floor() as i32;
    if loss <= 0 {
        return;
    }
    award_xp(server, conn, -loss);
}

/// Run the server-authoritative death of a player: apply the xp penalty (which
/// may de-level) and the on-death state resets the client's `DeathBroadcast`
/// used to drive. Idempotency (the `death_processed` flag) and the EntityDied
/// fan are the caller's job, since it owns the recipient list and the sweep.
pub fn kill_player(server: &mut RenetServer, conn: &mut PerConnection) {
    apply_death_penalty(server, conn);
    // On-death resets — mirror the old DeathBroadcast handler so a
    // server-detected death looks identical to the client-driven one. Buffs
    // clear here too: they're server-authoritative, so without this the next
    // BuffSnapshot fan would restore them onto the corpse.
    conn.hp = 0.0;
    conn.cast_spell_name.clear();
    conn.cast_total_duration = 0.0;
    conn.cast_set_at = None;
    conn.active_buffs.clear();
    conn.camp_since = None; // a corpse can't make camp
    // Flag a corpse for the tick's corpse-creation pass. Set on EVERY death path
    // (this is called from the server-detected death sweep AND the client-first
    // DeathBroadcast handler), so fall damage / Trigger Death / a dying linkdead
    // body all leave a corpse, not just server-simulated combat kills.
    conn.corpse_pending = true;
    super::regen::mark_dirty(conn);
}

/// Pure xp/level resolver: given accumulated `xp` (progress into `level`) that
/// may now be over the band or negative, fold it into a final
/// `(xp, level, xp_to_next)`. Level-ups spill the remainder into the next
/// band; level-downs borrow from the previous band but stop at
/// [`DEATH_PENALTY_FLOOR_LEVEL`] (any leftover deficit there is clamped to 0).
fn resolve(mut xp: i32, mut level: i32) -> (i32, i32, i32) {
    let mut xp_to_next = char_data::xp_to_next_for(level);
    // Level up: carry the overflow into successive bands.
    while xp >= xp_to_next {
        xp -= xp_to_next;
        level += 1;
        xp_to_next = char_data::xp_to_next_for(level);
    }
    // Level down: borrow the previous (smaller) band, floored at level 5.
    while xp < 0 && level > DEATH_PENALTY_FLOOR_LEVEL {
        level -= 1;
        xp_to_next = char_data::xp_to_next_for(level);
        xp += xp_to_next;
    }
    if xp < 0 {
        xp = 0; // hit the floor; cannot de-level below level 5
    }
    (xp, level, xp_to_next)
}

/// Add the intrinsic (gear-free, buff-free) per-level stat delta between two
/// levels onto `conn`'s running totals. Adds only the delta so equipment and
/// buff bonuses already in those totals survive. De-leveling can pull a max
/// below the current value, so current resources are clamped down.
fn apply_intrinsic_delta(conn: &mut PerConnection, old_level: i32, new_level: i32) {
    let old = char_data::compute(&conn.race, &conn.class, old_level);
    let new = char_data::compute(&conn.race, &conn.class, new_level);

    conn.strength += new.stats.strength - old.stats.strength;
    conn.dexterity += new.stats.dexterity - old.stats.dexterity;
    conn.agility += new.stats.agility - old.stats.agility;
    conn.intelligence += new.stats.intelligence - old.stats.intelligence;
    conn.wisdom += new.stats.wisdom - old.stats.wisdom;
    conn.charisma += new.stats.charisma - old.stats.charisma;
    conn.constitution += new.stats.constitution - old.stats.constitution;

    conn.max_hp += new.max_hp - old.max_hp;
    conn.max_mp += new.max_mp - old.max_mp;
    conn.max_stamina += new.max_stamina - old.max_stamina;

    conn.hp = conn.hp.min(conn.max_hp);
    conn.mp = conn.mp.min(conn.max_mp);
    conn.stamina = conn.stamina.min(conn.max_stamina);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bands at the default curve: L1=100, L2=150, L3=225, L4=337, L5=505,
    // L6=757 (each = prev * 1.5, truncated). resolve() must match.

    #[test]
    fn no_change_when_xp_in_band() {
        // 50/100 into level 5 stays put.
        assert_eq!(resolve(50, 5), (50, 5, char_data::xp_to_next_for(5)));
    }

    #[test]
    fn single_level_up_carries_remainder() {
        // Level 5 band is 505; 505 + 30 over → level 6 with 30 left.
        let band5 = char_data::xp_to_next_for(5);
        let (xp, level, to_next) = resolve(band5 + 30, 5);
        assert_eq!(level, 6);
        assert_eq!(xp, 30);
        assert_eq!(to_next, char_data::xp_to_next_for(6));
    }

    #[test]
    fn multi_level_up_in_one_award() {
        // A huge grant from level 5 should climb several bands.
        let big = char_data::xp_to_next_for(5)
            + char_data::xp_to_next_for(6)
            + char_data::xp_to_next_for(7)
            + 10;
        let (xp, level, _) = resolve(big, 5);
        assert_eq!(level, 8);
        assert_eq!(xp, 10);
    }

    #[test]
    fn de_level_borrows_previous_band() {
        // 10 into level 6, lose 30 → drop to level 5 with band5 - 20 left.
        let band5 = char_data::xp_to_next_for(5);
        let (xp, level, to_next) = resolve(10 - 30, 6);
        assert_eq!(level, 5);
        assert_eq!(to_next, band5);
        assert_eq!(xp, band5 - 20);
    }

    #[test]
    fn de_level_floors_at_level_5() {
        // Deep deficit at level 5 cannot drop below 5; xp clamps to 0.
        let (xp, level, to_next) = resolve(-99999, 5);
        assert_eq!(level, 5);
        assert_eq!(xp, 0);
        assert_eq!(to_next, char_data::xp_to_next_for(5));
    }

    #[test]
    fn de_level_cascade_stops_at_floor() {
        // From level 7, a deficit larger than bands 7+6 lands exactly on the
        // floor (level 5, xp 0) rather than continuing past it.
        let deficit = char_data::xp_to_next_for(7) + char_data::xp_to_next_for(6) + 50;
        let (xp, level, _) = resolve(-deficit, 7);
        assert_eq!(level, 5);
        assert_eq!(xp, 0);
    }

    #[test]
    fn death_loss_is_five_percent_of_band() {
        // Sanity on the penalty fraction math used by apply_death_penalty.
        let band6 = char_data::xp_to_next_for(6);
        let loss = (band6 as f32 * DEATH_XP_LOSS_FRACTION).floor() as i32;
        assert_eq!(loss, (band6 * 5) / 100);
    }
}
