//! Swing pacing: the swing-rate limit's rules (how often an honest client can swing
//! each hand, at its haste) and the skill-ups and multipliers a landed swing carries.
//! Auto-attack is paced by the client, so the server floors how fast the same hand may
//! swing or a modified client could spam Attack; the numbers mirror the client's pacing
//! in `autoloads/combat.gd` and must stay in step with it.

use super::*;

/// Fist (empty-hand) attack delay in seconds — the client's fallback when no
/// weapon is equipped (`combat.gd::_get_weapon_delay` returns 2.0).
pub(super) const FIST_WEAPON_DELAY: f32 = 2.0;

/// Off-hand swings are slower than main-hand by this factor (client
/// `OFFHAND_DELAY_MULT`).
pub(super) const OFFHAND_DELAY_MULT: f32 = 1.5;

/// The strongest haste the gate honours, however many Haste buffs the server
/// holds for a player: a cap on their sum (the Enchanter's 30% and a song's
/// 10% are the most the data stacks), never an assumption about anyone.
/// Until 2026-10-30 the gate assumed everyone had this much, since player
/// haste was client-paced; a modified client could swing at twice an
/// unhasted honest rate. It reads each player's real haste now.
pub(super) const MAX_MODELED_HASTE: f32 = 0.5;

/// The client's own floor on a swing interval (`combat.gd`'s `maxf(0.5, ...)`).
pub(super) const CLIENT_MIN_SWING_SECS: f32 = 0.5;

/// Tolerance subtracted from the computed minimum to absorb the 20 Hz tick, UDP
/// bunching, and Godot timer granularity — so honest jitter never trips it.
/// Sized for the tightest legit case: a fully-hasted player on the fastest
/// weapon swings its main hand every 1.0s, and arrival gaps compress under
/// real-internet jitter (mobile / congested / transcontinental). 0.35s keeps
/// that player's margin comfortably above realistic jitter once the target
/// server is remote (schedule Phase 2), at the cost of letting a forged client
/// swing at most ~1.5x the fully-hasted rate — a bounded residual, since the
/// gate's job is to kill wire-speed spam, not enforce exact cadence.
pub(super) const SWING_RATE_GRACE_SECS: f32 = 0.35;

/// Absolute per-hand swing floor. The client's `maxf(0.5, ...)` makes any
/// same-hand interval below ~0.5s physically impossible for an honest client, so
/// 0.4s (0.5 minus jitter) is a safe unconditional forgery line.
pub(super) const HARD_MIN_SWING_SECS: f32 = 0.4;

/// The fastest same-hand interval an honest client paces at: `weapon_delay`
/// (fists 2.0) times the off-hand multiplier, scaled by the player's haste
/// (`combat.gd::_update_attack_interval`, which reads the Haste buff the
/// server fans; the server sums its Haste buffs, which can only be as
/// lenient as the client's single strongest), floored at the client's own
/// 0.5 s. The client sends one Attack per swing at this cadence with no burst
/// or catch-up, and restarts the interval on a re-target or a toggle, so real
/// arrivals are already >= it.
pub(super) fn honest_swing_pace(weapon_delay: f32, is_offhand: bool, haste: f32) -> f32 {
    let hand_mult = if is_offhand { OFFHAND_DELAY_MULT } else { 1.0 };
    let haste = haste.clamp(0.0, MAX_MODELED_HASTE);
    (weapon_delay * hand_mult * (1.0 - haste)).max(CLIENT_MIN_SWING_SECS)
}

/// Minimum wall-clock seconds allowed between two SAME-HAND swings before the
/// faster one is a forgery: the honest pace minus a jitter grace, floored at
/// `HARD_MIN_SWING_SECS`. Only ever used to reject "too fast", never "too
/// slow". The pace is computed at `carried_haste`, which covers the client's
/// timer lag; the weapon does not carry: a client alternating a fast and a
/// slow weapon every swing would otherwise swing the slow one at the fast
/// one's pace indefinitely (the 2026-10-30 review); the client re-times its
/// swing when a weapon changes instead.
pub(super) fn min_swing_interval_secs(pace: f32) -> f32 {
    (pace - SWING_RATE_GRACE_SECS).max(HARD_MIN_SWING_SECS)
}

/// The haste a same-hand swing is paced at: the greatest of the haste now
/// and the haste of the hand's last two ACCEPTED swings (newest first).
/// Two, not one, because the client's attack timer lags by two: a Godot
/// Timer reloads its `wait_time` before the timeout handler runs, so a wait
/// time set in the handler of swing N first times the gap from N+1 to N+2
/// (measured 2026-11-01: a 1.4 s timer changed to 2.0 fired after 1.4, 1.4,
/// then 2.0), and a fresh `start()` after a stop runs the wait time set at
/// the last swing before the stop. So a Haste that ran out costs no honest
/// swing, whether it ends mid-fight or between fights, and a forged client
/// gains at most two swings at the expired haste, since every accepted swing
/// pushes its own haste into the history.
pub(super) fn carried_haste(haste_now: f32, history: [f32; 2]) -> f32 {
    haste_now.max(history[0]).max(history[1])
}

/// Records an accepted swing's haste in the hand's history, newest first.
pub(super) fn stamp_swing_haste(history: &mut [f32; 2], haste_now: f32) {
    *history = [haste_now, history[0]];
}

/// True if a same-hand swing arriving `now` is faster than `min_interval` since
/// that hand's last accepted swing (`last`). A hand that has never swung
/// (`None`) is always allowed. Split out so the accept/reject decision is unit
/// testable without the whole attack loop.
pub(super) fn swing_too_fast(last: Option<Instant>, now: Instant, min_interval: f32) -> bool {
    last.is_some_and(|t| now.duration_since(t).as_secs_f32() < min_interval)
}

/// Track 18.1 — a swing at a mob trains the hand's weapon skill and defense,
/// hit or miss, and an off-hand swing trains dual wield too (as the client
/// always did); tells the attacker privately when a roll lands. The skill key
/// comes from `combat::weapon_skill_key` (the item's skill, or hand_to_hand).
pub(super) fn train_swing_skills(
    server: &mut RenetServer,
    connections: &mut HashMap<ClientId, PerConnection>,
    attacker_cid: ClientId,
    weapon_skill_key: &str,
    is_offhand: bool,
) {
    let Some(att) = connections.get_mut(&attacker_cid) else {
        return;
    };
    let keys: &[&str] = if is_offhand {
        &[weapon_skill_key, "defense", "dual_wield"]
    } else {
        &[weapon_skill_key, "defense"]
    };
    for &key in keys {
        if let Some(new_score) = skills::try_advance(att, skills::Skill::Weapon, key) {
            handlers::send_skill_progress_update(
                server,
                attacker_cid,
                skills::Skill::Weapon.as_protocol(),
                key.to_string(),
                new_score,
            );
        }
    }
}

/// An active skill's swing (client-authority audit item 6): the ordinary
/// roll times the skill's multiplier, never below one point; Truesight
/// Strike never misses. A plain swing passes through unchanged.
pub(super) fn skill_swing(swing: combat::Swing, skill: Option<&SkillSwing>) -> combat::Swing {
    let Some(sk) = skill else { return swing };
    let mut out = swing;
    out.amount = ((swing.amount.max(0) as f32) * sk.mult).round().max(1.0) as i32;
    if sk.effect == active_skills::Effect::Truesight {
        out.missed = false;
    }
    out
}

#[cfg(test)]
mod tests {
    // Melee swing-rate limit: the per-hand minimum interval the server allows
    // between same-hand swings. It must (a) sit comfortably BELOW the fastest a
    // legit client swings for each weapon AT ITS OWN HASTE (so no false
    // positives), (b) never fall below the absolute hard floor, and (c) read
    // the player's real haste, so an unhasted forged client no longer gets the
    // fully-hasted allowance. Mirror of combat.gd pacing (weapon_delay x
    // offhand-mult x (1-haste), maxf 0.5).
    #[test]
    fn swing_rate_min_interval_is_below_client_fastest_but_above_floor() {
        use super::{honest_swing_pace, min_swing_interval_secs, HARD_MIN_SWING_SECS, OFFHAND_DELAY_MULT};
        let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;
        let gate = |delay: f32, off: bool, haste: f32| {
            min_swing_interval_secs(honest_swing_pace(delay, off, haste))
        };
        // Fists, main hand, no haste: 2.0 minus the 0.35 grace = 1.65. (The
        // old gate gave everyone the 50% allowance, 0.65, hasted or not.)
        assert!(approx(gate(2.0, false, 0.0), 1.65));
        // The Enchanter's 30%: 2.0 * 0.7 = 1.4, minus grace = 1.05.
        assert!(approx(gate(2.0, false, 0.3), 1.05));
        // The cap: 2.0 * 0.5 = 1.0, minus grace = 0.65; a sum past it reads as the cap.
        assert!(approx(gate(2.0, false, 0.5), 0.65));
        assert!(approx(gate(2.0, false, 0.9), gate(2.0, false, 0.5)));
        // Off hand 2.0, no haste: 2.0 * 1.5 = 3.0, minus grace = 2.65.
        assert!(approx(gate(2.0, true, 0.0), 2.65));
        // Slow war axe 3.2 main, no haste: 2.85.
        assert!(approx(gate(3.2, false, 0.0), 2.85));
        // The gate must ALWAYS be <= the client's fastest legit same-hand interval
        // at that haste (weapon_delay * hand_mult * (1-haste), floored 0.5
        // client-side) so a legit swing is never rejected. Sweep the real range
        // PLUS hypothetical fast weapons (<2.0) where the client's 0.5s floor and
        // the server's 0.4s floor diverge most.
        for &haste in &[0.0_f32, 0.1, 0.3, 0.5] {
            for &delay in &[1.0_f32, 1.4, 2.0, 2.5, 2.8, 3.0, 3.2] {
                for &off in &[false, true] {
                    let hand_mult = if off { OFFHAND_DELAY_MULT } else { 1.0 };
                    let client_fastest = (delay * hand_mult * (1.0 - haste)).max(0.5);
                    assert!(
                        gate(delay, off, haste) <= client_fastest,
                        "gate {} must be <= client fastest {} (delay {delay}, off {off}, haste {haste})",
                        gate(delay, off, haste),
                        client_fastest
                    );
                }
            }
        }
        // Never below the absolute floor, even for an implausibly fast weapon.
        assert!(gate(0.1, false, 0.5) >= HARD_MIN_SWING_SECS);
        assert!(gate(0.0, true, 0.0) >= HARD_MIN_SWING_SECS);
    }

    // The weapon never carries: a staff swung after a dagger is held to the
    // staff's own gate at once (the review of 2026-10-30: alternating the two
    // every swing would otherwise swing the staff at the dagger's pace for
    // good), and a buff landing between two swings only ever loosens the gate.
    #[test]
    fn the_carried_haste_covers_the_timed_swing_but_the_weapon_does_not() {
        use super::{carried_haste, honest_swing_pace, min_swing_interval_secs};
        let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;
        let gate = |delay: f32, haste_now: f32, history: [f32; 2]| {
            min_swing_interval_secs(honest_swing_pace(delay, false, carried_haste(haste_now, history)))
        };
        assert!(approx(gate(2.0, 0.0, [0.3, 0.3]), 1.05), "the swing timed under haste passes");
        assert!(approx(gate(2.0, 0.0, [0.0, 0.3]), 1.05), "and the one after it, timed two swings back");
        assert!(approx(gate(2.0, 0.0, [0.0, 0.0]), 1.65), "the third is held to the unhasted gate");
        assert!(approx(gate(3.2, 0.0, [0.0, 0.0]), 2.85), "a staff after a dagger is held to the staff's gate at once");
        assert!(approx(gate(1.4, 0.0, [0.0, 0.0]), 1.05), "and the dagger after the staff to the dagger's");
        assert!(gate(2.0, 0.3, [0.0, 0.0]) <= gate(2.0, 0.0, [0.0, 0.0]), "a buff landing between swings only loosens the gate");
    }

    // The client's attack timer as a Godot Timer runs it (measured
    // 2026-11-01): at a timeout the timer reloads its CURRENT wait_time, then
    // the handler sends the swing and sets a new wait_time for the haste it
    // holds; `start()` after a stop runs the current wait_time. Each fight is
    // the haste held at each of its swings, the fights ten minutes apart.
    // Returns how many of those honest swings the server's rule rejects;
    // `one_deep` judges by the rule built on 2026-10-30, which carried one.
    fn rejected_honest_swings(delay: f32, fights: &[&[f32]], one_deep: bool) -> usize {
        use super::{carried_haste, honest_swing_pace, min_swing_interval_secs, stamp_swing_haste};
        let mut wait_time = honest_swing_pace(delay, false, 0.0);
        let mut t = 0.0_f32;
        let mut last_at: Option<f32> = None;
        let mut history = [0.0_f32; 2];
        let mut rejected = 0;
        for fight in fights {
            t += 600.0;
            let mut gap = wait_time;
            for &haste in fight.iter() {
                t += gap;
                let carried = if one_deep { haste.max(history[0]) } else { carried_haste(haste, history) };
                let min = min_swing_interval_secs(honest_swing_pace(delay, false, carried));
                if last_at.is_some_and(|l| t - l < min) {
                    rejected += 1;
                } else {
                    last_at = Some(t);
                    stamp_swing_haste(&mut history, haste);
                }
                gap = wait_time;
                wait_time = honest_swing_pace(delay, false, haste);
            }
        }
        rejected
    }

    // The 2026-11-01 finding (WAROI, bare-handed: a Haste that ran out
    // between fights cost the second swing of the next fight), the same
    // expiry mid-fight, and a Haste landing mid-fight: no honest swing is
    // rejected, where the one-swing carry rejected one in each expiry case.
    #[test]
    fn no_honest_swing_is_rejected_when_a_haste_ends_or_begins() {
        let hasted: &[f32] = &[0.3; 6];
        let unhasted: &[f32] = &[0.0; 6];
        let ends_mid_fight: &[f32] = &[0.3, 0.3, 0.3, 0.0, 0.0, 0.0, 0.0];
        let begins_mid_fight: &[f32] = &[0.0, 0.0, 0.3, 0.3, 0.3, 0.3];
        for delay in [2.0_f32, 3.2, 1.4] {
            assert_eq!(rejected_honest_swings(delay, &[hasted, unhasted], false), 0, "between fights, delay {delay}");
            assert_eq!(rejected_honest_swings(delay, &[ends_mid_fight], false), 0, "mid-fight, delay {delay}");
            assert_eq!(rejected_honest_swings(delay, &[begins_mid_fight], false), 0, "a haste landing, delay {delay}");
            assert_eq!(rejected_honest_swings(delay, &[unhasted, hasted, unhasted], false), 0, "fights both ways, delay {delay}");
        }
        assert_eq!(rejected_honest_swings(2.0, &[hasted, unhasted], true), 1, "the one-swing carry dropped WAROI's swing");
        assert_eq!(rejected_honest_swings(2.0, &[ends_mid_fight], true), 1, "and one mid-fight");
    }

    // The bound on a forged client: once its Haste is gone, swinging on at
    // the hasted pace gets at most two swings through at that pace, then is
    // held to the unhasted gate.
    #[test]
    fn a_forged_client_gains_at_most_two_swings_at_an_expired_haste() {
        use super::{carried_haste, honest_swing_pace, min_swing_interval_secs, stamp_swing_haste};
        let hasted_gap = honest_swing_pace(2.0, false, 0.3);
        let mut history = [0.3_f32, 0.3];
        let mut last_at = 0.0_f32;
        let mut t = 0.0_f32;
        let mut at_the_hasted_pace = 0;
        for _ in 0..20 {
            t += hasted_gap;
            let min = min_swing_interval_secs(honest_swing_pace(2.0, false, carried_haste(0.0, history)));
            if t - last_at >= min {
                if t - last_at < 1.65 - 1e-4 {
                    at_the_hasted_pace += 1;
                }
                last_at = t;
                stamp_swing_haste(&mut history, 0.0);
            }
        }
        assert_eq!(at_the_hasted_pace, 2);
    }

    // The per-hand accept/reject decision: a never-swung hand is always allowed;
    // a swing within min_interval is "too fast" (rejected); one at/after it is
    // allowed. Per-hand independence is by construction (last_swing_at is indexed
    // by is_offhand), so a main + off swing at the same instant never interfere.
    #[test]
    fn swing_too_fast_gates_only_within_the_interval() {
        use super::swing_too_fast;
        use std::time::Duration;
        let base = std::time::Instant::now() + Duration::from_secs(60);
        // Never swung -> always accepted.
        assert!(!swing_too_fast(None, base, 0.65));
        // 0.5s after last, min 0.65 -> too fast (rejected).
        assert!(swing_too_fast(Some(base - Duration::from_millis(500)), base, 0.65));
        // Exactly at the interval -> allowed (not "less than").
        assert!(!swing_too_fast(Some(base - Duration::from_millis(650)), base, 0.65));
        // Well after -> allowed.
        assert!(!swing_too_fast(Some(base - Duration::from_secs(2)), base, 0.65));
    }
}
