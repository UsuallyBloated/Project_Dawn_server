//! The pure decisions behind the CastSpell gate in `tick.rs`, kept here so
//! they can be unit-tested without a world: whether a timed cast arrived
//! early, on time, or so late it was being held, and whether an interrupt
//! that landed after the cast began beats a completion in the same tick.
//! Spell batch step 0 (2026-10-08).

use std::time::Instant;

/// Network jitter allowance: packet loss can push a CastSpell's arrival past
/// the cast bar's end on the client, but a forged CastSpell sent the same
/// tick as its CastStart falls well short.
pub const CAST_TOLERANCE_MS: u128 = 100;

/// How long after the bar's end a CastSpell is still honoured. Past this the
/// cast was being HELD: a finished bar kept in hand and released when
/// convenient (a pre-cast heal fired the instant it is needed). Two seconds
/// covers any honest latency spike; an honest client sends the moment the
/// bar ends.
pub const CAST_HOLD_GRACE_MS: u128 = 2_000;

#[derive(Debug, PartialEq, Eq)]
pub enum CastTiming {
    /// No CastStart on file for this spell, or the bar has not run long enough.
    NotReady,
    /// Arrived within the window after the bar's end.
    Ready,
    /// The bar ended more than `CAST_HOLD_GRACE_MS` ago.
    Held,
}

/// Where a timed cast's release falls relative to its bar.
/// `elapsed_ms` is None when no CastStart is on file.
pub fn cast_timing(elapsed_ms: Option<u128>, required_ms: u128) -> CastTiming {
    match elapsed_ms {
        None => CastTiming::NotReady,
        Some(e) if e + CAST_TOLERANCE_MS < required_ms => CastTiming::NotReady,
        Some(e) if e > required_ms + CAST_HOLD_GRACE_MS => CastTiming::Held,
        Some(_) => CastTiming::Ready,
    }
}

/// True when an interrupt landed at or after the moment this cast's bar
/// began: the interrupt wins, even if both are processed in the same tick.
/// The gate reads the cast cache as it was at dispatch (so a same-batch
/// CastComplete cannot blank it), which also meant a hit processed earlier
/// in the tick had already cleared the live cache and the cast still went
/// through. An interrupt before this bar began belongs to an earlier cast.
pub fn beaten_by_interrupt(interrupted_at: Option<Instant>, bar_began_at: Option<Instant>) -> bool {
    match (interrupted_at, bar_began_at) {
        (Some(hit), Some(began)) => hit >= began,
        _ => false,
    }
}

/// What an interrupted cast costs (spell batch step 3, user call of
/// 2026-10-05): the spell's mana in proportion to how far the bar had run,
/// capped at what the caster has. A hit that did no damage (absorbed, or a
/// zero roll) charges nothing, or a damage shield and a zero-damage swing
/// would be mana-drain weapons; a deliberate cancel never reaches the server
/// and so costs nothing either. `cast_time_s` is the server's own cast time
/// for the spell, never the client's bar length: a forged long bar would
/// otherwise shrink the fraction to nothing. An instant cast has no bar to
/// interrupt.
pub fn interrupt_charge(
    mana_cost: f32,
    cast_time_s: f32,
    elapsed_ms: Option<u128>,
    damage_applied: i32,
    mp_now: f32,
) -> f32 {
    if damage_applied <= 0 || cast_time_s <= 0.0 || mana_cost <= 0.0 {
        return 0.0;
    }
    let Some(elapsed_ms) = elapsed_ms else {
        return 0.0;
    };
    let fraction = (elapsed_ms as f32 / (cast_time_s * 1000.0)).clamp(0.0, 1.0);
    (mana_cost * fraction).min(mp_now).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn an_interrupt_charges_mana_in_proportion_to_the_bar() {
        // A 40 mana, 4 s spell.
        assert_eq!(interrupt_charge(40.0, 4.0, Some(2_000), 7, 100.0), 20.0, "half way, half the mana");
        assert_eq!(interrupt_charge(40.0, 4.0, Some(1_000), 7, 100.0), 10.0, "a quarter in");
        assert_eq!(interrupt_charge(40.0, 4.0, Some(9_000), 7, 100.0), 40.0, "past the end: the whole cost, never more");
        assert_eq!(interrupt_charge(40.0, 4.0, Some(2_000), 0, 100.0), 0.0, "a zero-damage hit charges nothing");
        assert_eq!(interrupt_charge(40.0, 4.0, Some(2_000), -3, 100.0), 0.0, "an absorbed hit charges nothing");
        assert_eq!(interrupt_charge(40.0, 4.0, None, 7, 100.0), 0.0, "no bar on file");
        assert_eq!(interrupt_charge(40.0, 0.0, Some(2_000), 7, 100.0), 0.0, "an instant cast has no bar");
        assert_eq!(interrupt_charge(40.0, 4.0, Some(2_000), 7, 12.5), 12.5, "capped at the mana held");
        assert_eq!(interrupt_charge(40.0, 4.0, Some(2_000), 7, 0.0), 0.0, "an empty bar stays at zero");
    }

    #[test]
    fn a_cast_is_ready_only_inside_its_window() {
        let required = 2_000;
        assert_eq!(cast_timing(None, required), CastTiming::NotReady, "no bar on file");
        assert_eq!(cast_timing(Some(0), required), CastTiming::NotReady, "sent with the start");
        assert_eq!(cast_timing(Some(1_899), required), CastTiming::NotReady, "just early");
        assert_eq!(cast_timing(Some(1_900), required), CastTiming::Ready, "inside the jitter allowance");
        assert_eq!(cast_timing(Some(2_000), required), CastTiming::Ready, "on time");
        assert_eq!(cast_timing(Some(4_000), required), CastTiming::Ready, "the last honoured millisecond");
        assert_eq!(cast_timing(Some(4_001), required), CastTiming::Held, "held past the bar");
        assert_eq!(cast_timing(Some(60_000), required), CastTiming::Held, "held a minute");
    }

    #[test]
    fn an_interrupt_after_the_bar_began_beats_the_cast() {
        let began = Instant::now();
        let before = began - Duration::from_millis(500);
        let after = began + Duration::from_millis(500);
        assert!(!beaten_by_interrupt(None, Some(began)), "never interrupted");
        assert!(!beaten_by_interrupt(Some(after), None), "an instant cast has no bar to beat");
        assert!(!beaten_by_interrupt(Some(before), Some(began)), "an older cast's interrupt");
        assert!(beaten_by_interrupt(Some(began), Some(began)), "the same instant counts");
        assert!(beaten_by_interrupt(Some(after), Some(began)), "hit mid-bar");
    }
}
