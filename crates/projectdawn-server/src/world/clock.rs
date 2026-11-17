//! The world clock: one shared time of day for everyone in the world.
//!
//! The hour is derived from the wall clock instead of being counted up from
//! boot, which buys three things at once: every client is told the same
//! hour, a server restart does not reset the day to morning, and there is
//! nothing to persist. The server sends the hour to a player as they enter
//! the world and to everyone on a slow cadence; each client runs its own
//! clock at the same rate in between and only uses the broadcasts to stay in
//! step (`autoloads/time_of_day.gd` in the client repo).
//!
//! One rule on the server reads the hour: the night animals of the roam table
//! are alive only at night (`is_night`, read by the spawner). The sky itself
//! is cosmetic. Any rule that depends on the hour asks this module, never a
//! client.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Real seconds in one game day (20 minutes). LOCKSTEP with the client's
/// `DAY_DURATION` in `autoloads/time_of_day.gd`: the client advances its own
/// clock at this rate between broadcasts, so if the two disagree the sky
/// visibly corrects itself once a minute.
pub const DAY_LENGTH_SECS: f64 = 1200.0;

/// How often the hour is re-sent to everyone in the world. A game hour is
/// 50 real seconds at the day length above, and clients keep time themselves
/// between sends, so this is drift correction, not animation.
pub const BROADCAST_INTERVAL: Duration = Duration::from_secs(60);

/// Night runs from `NIGHT_START_HOUR` through midnight to `NIGHT_END_HOUR`:
/// the hours the client's sky is fully dark. LOCKSTEP with the client's
/// `NIGHT_START_HOUR` and `NIGHT_END_HOUR` in `autoloads/time_of_day.gd`
/// (`night_factor`), so the night animals are out exactly while it looks like
/// night. At the day length above a night is 400 real seconds.
pub const NIGHT_START_HOUR: f32 = 21.0;
pub const NIGHT_END_HOUR: f32 = 5.0;

/// Whether `hour` (in `[0, 24)`) is night. Keep-only-when-night, so an hour
/// that is not finite is day (the range form clippy offers would call it
/// night).
#[allow(clippy::manual_range_contains)]
pub fn is_night(hour: f32) -> bool {
    hour >= NIGHT_START_HOUR || hour < NIGHT_END_HOUR
}

/// The game hour, in `[0, 24)`, at `unix_secs` seconds since the Unix epoch.
pub fn hour_at(unix_secs: f64) -> f32 {
    let day_fraction = unix_secs.rem_euclid(DAY_LENGTH_SECS) / DAY_LENGTH_SECS;
    let hour = (day_fraction * 24.0) as f32;
    // The f64 fraction is below 1.0, but narrowing to f32 can round the last
    // instant of the day up to 24.0 exactly; that instant is midnight.
    if hour >= 24.0 {
        0.0
    } else {
        hour
    }
}

/// The game hour right now.
pub fn current_hour() -> f32 {
    // A system clock set before 1970 is the only way this fails; midnight is
    // as good an answer as any for a machine in that state.
    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    hour_at(unix_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_day_starts_at_midnight_and_wraps() {
        assert_eq!(hour_at(0.0), 0.0);
        assert_eq!(hour_at(DAY_LENGTH_SECS), 0.0);
        assert_eq!(hour_at(DAY_LENGTH_SECS * 1_000_000.0), 0.0);
    }

    #[test]
    fn night_is_nine_in_the_evening_to_five_in_the_morning() {
        for (hour, night) in [
            (0.0, true),
            (4.99, true),
            (5.0, false),
            (12.0, false),
            (20.99, false),
            (21.0, true),
            (23.99, true),
        ] {
            assert_eq!(is_night(hour), night, "hour {hour}");
        }
        assert!(!is_night(f32::NAN));
        // A third of the day: 400 real seconds at the day length.
        let night_hours = (24.0 - NIGHT_START_HOUR) + NIGHT_END_HOUR;
        assert_eq!(night_hours as f64 / 24.0 * DAY_LENGTH_SECS, 400.0);
    }

    #[test]
    fn an_hour_is_a_twenty_fourth_of_the_day() {
        let one_hour = DAY_LENGTH_SECS / 24.0;
        assert!((hour_at(one_hour) - 1.0).abs() < 1e-4);
        assert!((hour_at(DAY_LENGTH_SECS / 2.0) - 12.0).abs() < 1e-4);
        assert!((hour_at(DAY_LENGTH_SECS * 7.0 + one_hour * 18.5) - 18.5).abs() < 1e-3);
    }

    #[test]
    fn the_hour_is_always_inside_the_day() {
        // Walk a real-sized timestamp range in awkward steps, plus the
        // instants just either side of a day boundary.
        let base = 1_790_000_000.0_f64;
        let mut t = base;
        while t < base + DAY_LENGTH_SECS * 3.0 {
            let h = hour_at(t);
            assert!((0.0..24.0).contains(&h), "hour {h} at t = {t}");
            t += 0.37;
        }
        let boundary = (base / DAY_LENGTH_SECS).ceil() * DAY_LENGTH_SECS;
        for t in [boundary - 1e-6, boundary, boundary + 1e-6] {
            let h = hour_at(t);
            assert!((0.0..24.0).contains(&h), "hour {h} at the boundary t = {t}");
        }
    }

    #[test]
    fn two_reads_a_moment_apart_agree() {
        // What two clients entering the world together are told: the same
        // hour, give or take the instant between the two sends.
        let a = current_hour();
        let b = current_hour();
        let diff = (b - a).rem_euclid(24.0);
        assert!(diff < 0.01, "hour moved {diff} between two immediate reads");
    }
}
