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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
