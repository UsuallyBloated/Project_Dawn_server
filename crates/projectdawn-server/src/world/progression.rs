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
use super::skills::MAX_LEVEL;

/// Death penalty parameters, locked with the user 2026-06-22:
/// a death costs 5% of the current level's XP band, the loss cascades past the
/// level boundary with no per-death cap, and levels 1 to 4 are exempt while a
/// cascade can never drop a character below level 5 (grace line == floor line).
pub const DEATH_XP_LOSS_FRACTION: f32 = 0.05;
pub const DEATH_PENALTY_FLOOR_LEVEL: i32 = 5;

/// Per-kill XP follows EverQuest's QUADRATIC award, separate from the cubic level
/// curve (`char_data::xp_to_next_for`): a kill is worth `mob_level^2 * ZEM *
/// 35/10`. Pairing a quadratic reward with a cubic cost keeps kills-per-level
/// roughly constant (~11 on an even-con kill: band(L) ~ 3*L^2*1000 over a kill ~
/// L^2*262.5). `ZEM` is EQ's per-zone "Zone Experience Modifier" (P99 scale: 75
/// normal, ~80 dungeon, ~100 newbie); one global value for now, per-zone ZEM is a
/// later content pass. See `docs/design/everquest_xp_curve_reference.md`.
pub const ZEM_NORMAL: f32 = 75.0;
const ZEM_KILL_SCALE: f64 = 3.5; // EQEmu's 35/10 calibration

/// XP for killing a `mob_level` mob in a zone with modifier `zem` (pass
/// [`ZEM_NORMAL`] as the default). Floored at 1 so every kill is worth something.
/// NOTE: quest objective counting (PD_W0024, inside `award_kill`) rides the
/// same `xp > 0` gate at the tick.rs kill sites, so this floor is also what
/// keeps "kill N X" objectives advancing — if a future con-color rule lets
/// this return 0, hoist the quest counting out of those blocks.
pub fn kill_xp(mob_level: i32, zem: f32) -> i32 {
    let l = mob_level.max(1) as f64;
    (l * l * zem as f64 * ZEM_KILL_SCALE).round().max(1.0) as i32
}

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

    // saturating_add, not `+=`: a near-full band plus a large quest grant could
    // overflow i32 (a debug panic / release wrap to negative). Saturating then
    // lets resolve() fold the (clamped) total into the capped level cleanly.
    conn.xp = conn.xp.saturating_add(amount);
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
    // Reset first so a grace / zero-loss death stamps 0 on the corpse (the
    // corpse-creation pass reads this for the Slice 3 res refund).
    conn.death_lost_xp = 0;
    if conn.level < DEATH_PENALTY_FLOOR_LEVEL {
        return; // grace: levels 1 to 4 lose no xp on death
    }
    let loss = (conn.xp_to_next as f32 * DEATH_XP_LOSS_FRACTION).floor() as i32;
    if loss <= 0 {
        return;
    }
    let pre_level = conn.level;
    let pre_xp = conn.xp;
    award_xp(server, conn, -loss);
    // Store the ACTUAL xp removed (for the Slice 3 res refund), not the nominal
    // `loss`: at the level-5 floor a death is clamped (you can't de-level below 5),
    // so less than `loss` is really taken. Refunding a % of the nominal there would
    // hand back MORE than the death cost — net-positive xp from dying. When the
    // floor clamped us (same level, xp now 0) only the remaining progress was lost.
    conn.death_lost_xp = if conn.level == pre_level && conn.xp == 0 {
        pre_xp
    } else {
        loss
    };
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
    // Level up: carry the overflow into successive bands, but never past the
    // level cap. The old 1.5x curve capped leveling by accident — its band
    // saturated i32 at ~level 43, so xp (also i32) could never reach it. The
    // cubic curve stays well inside i32, so MAX_LEVEL is now the explicit cap.
    while xp >= xp_to_next && level < MAX_LEVEL {
        xp -= xp_to_next;
        level += 1;
        xp_to_next = char_data::xp_to_next_for(level);
    }
    // At the cap the bar sits full; any further overflow is discarded (there are
    // no levels beyond MAX_LEVEL — AA-style spend of surplus xp is a future system).
    if level >= MAX_LEVEL && xp > xp_to_next {
        xp = xp_to_next;
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

    // Bands at the default cubic curve: L1=1000, L2=7000, L3=19000, L4=37000,
    // L5=61000, L6=91000 (= L^3*1000 deltas, hell_mod 1.0 below 30). resolve()
    // must match.

    #[test]
    fn no_change_when_xp_in_band() {
        // 50/100 into level 5 stays put.
        assert_eq!(resolve(50, 5), (50, 5, char_data::xp_to_next_for(5)));
    }

    #[test]
    fn single_level_up_carries_remainder() {
        // Level 5 band is 61000; 30 past it (band5 + 30) lands at level 6, xp 30.
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

    #[test]
    fn level_up_caps_at_max_level() {
        // Even i32::MAX of grant can't climb past the cap; the bar sits full
        // (xp == band) at MAX_LEVEL instead of spilling into level 61+.
        let (xp, level, to_next) = resolve(i32::MAX, 5);
        assert_eq!(level, MAX_LEVEL);
        assert_eq!(xp, to_next);
    }

    #[test]
    fn band_never_overflows_i32() {
        // The cubic, clamped to the level cap, stays well inside i32. Unclamped
        // it overruns i32 near level ~70, so the clamp is the guard. Bands are
        // NOT monotonic: the level-59 "triple hell" (~89M) is the single largest
        // band, bigger than 60, but still ~24x under i32::MAX.
        for lvl in 1..=99 {
            let band = char_data::xp_to_next_for(lvl);
            assert!(band > 0, "band({lvl}) = {band} went non-positive");
            assert!(band < 100_000_000, "band({lvl}) = {band} unexpectedly large");
        }
        // Above the cap the band is pinned to the cap band, never the raw cubic
        // for that level (which would overflow i32).
        assert_eq!(
            char_data::xp_to_next_for(99),
            char_data::xp_to_next_for(MAX_LEVEL),
        );
    }

    #[test]
    fn kill_xp_is_quadratic_in_mob_level() {
        // mob_level^2 * 75 * 3.5: doubling the mob level ~4x the award.
        assert_eq!(kill_xp(1, ZEM_NORMAL), 263); // 262.5 rounds up
        assert_eq!(kill_xp(10, ZEM_NORMAL), 26_250);
        assert_eq!(kill_xp(20, ZEM_NORMAL), 105_000);
        assert!(kill_xp(0, ZEM_NORMAL) >= 1, "a kill is always worth >= 1");
    }
}
