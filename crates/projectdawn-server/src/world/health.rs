//! The world loop's own health: how long each tick's work takes, how long
//! the 60 s checkpoint holds the loop, and how many player steps the slope
//! rule refused, reported once a minute as the `world health` log line
//! beside `enemy position fan` and `player link`.
//!
//! A tick has `TICK_DT` (50 ms) to do its work. The loop awaits the database
//! inline (logins, the checkpoint, the per-tick quest and drop saves), so a
//! slow disk or a crowded world shows up here first, as a tick that ran past
//! its budget, long before players call it lag. The slope refusals are the
//! same line's eye on the height model (`terrain::player_step`): on flat
//! ground none can happen, so a count there is a modified client or a ground
//! the two sides disagree on; on hills a Move carries the player's input, not
//! what their body managed, so a key held into a cliff counts too, honest or
//! not. The numbers are for the operator: nothing in the game reads them.

use std::time::Duration;

use super::TICK_DT;

/// One report window's measurements, cleared by `take_report`.
#[derive(Default)]
pub struct TickHealth {
    /// Each tick's work time, in microseconds (a minute at 20 Hz is 1,200).
    tick_us: Vec<u32>,
    /// Each checkpoint's time in the window, in microseconds.
    checkpoint_us: Vec<u32>,
    /// The connections the window's checkpoints walked, summed.
    checkpoint_conns: usize,
    /// Player steps the slope rule refused in the window.
    slope_refusals: u64,
}

/// What one window of `TickHealth` measured, ready to log.
#[derive(Debug, PartialEq)]
pub struct HealthReport {
    pub ticks: usize,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    /// Ticks whose work alone took longer than `TICK_DT`: each one delayed
    /// the next tick for everyone.
    pub over_budget: usize,
    pub checkpoints: usize,
    pub checkpoint_max_ms: f64,
    pub checkpoint_conns: usize,
    /// Player steps the slope rule refused (one per tick per player held at
    /// a face). On flat ground this reads 0, and anything else there is a
    /// modified client, or a terrain the client has and the server does not
    /// (a scene edit nobody exported). On hills it also counts an honest
    /// player holding a key into a cliff: a Move carries the input, not what
    /// the body managed, so there it is a curiosity, not evidence.
    pub slope_refusals: u64,
}

impl HealthReport {
    /// A window worth a line even with nobody left in the world: a tick ran
    /// past its budget, or a step was refused (its sender may have logged
    /// out since, and the count must not go unreported).
    pub fn demands_a_line(&self) -> bool {
        self.over_budget > 0 || self.slope_refusals > 0
    }
}

fn micros(d: Duration) -> u32 {
    u32::try_from(d.as_micros()).unwrap_or(u32::MAX)
}

fn ms(us: u32) -> f64 {
    // One decimal place is all the log line needs.
    (f64::from(us) / 100.0).round() / 10.0
}

/// The nearest-rank percentile of an ascending slice (`p` in 0..=100).
fn percentile(sorted: &[u32], p: usize) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

impl TickHealth {
    pub fn record_tick(&mut self, work: Duration) {
        self.tick_us.push(micros(work));
    }

    pub fn record_checkpoint(&mut self, took: Duration, conns: usize) {
        self.checkpoint_us.push(micros(took));
        self.checkpoint_conns += conns;
    }

    /// One player step the slope rule refused (`terrain::player_step`).
    pub fn record_slope_refusal(&mut self) {
        self.slope_refusals += 1;
    }

    /// The window's report, and a fresh window.
    pub fn take_report(&mut self) -> HealthReport {
        let mut ticks = std::mem::take(&mut self.tick_us);
        let checkpoints = std::mem::take(&mut self.checkpoint_us);
        let checkpoint_conns = std::mem::take(&mut self.checkpoint_conns);
        let slope_refusals = std::mem::take(&mut self.slope_refusals);
        ticks.sort_unstable();
        let budget = micros(TICK_DT);
        HealthReport {
            ticks: ticks.len(),
            p50_ms: ms(percentile(&ticks, 50)),
            p99_ms: ms(percentile(&ticks, 99)),
            max_ms: ms(ticks.last().copied().unwrap_or(0)),
            over_budget: ticks.iter().filter(|&&us| us > budget).count(),
            checkpoints: checkpoints.len(),
            checkpoint_max_ms: ms(checkpoints.iter().copied().max().unwrap_or(0)),
            checkpoint_conns,
            slope_refusals,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_reads_the_window_and_starts_a_new_one() {
        let mut health = TickHealth::default();
        // 100 ticks: 1 ms to 100 ms, so the median is 50 ms, the 99th 99 ms,
        // and the 50 ticks past 50 ms ran over budget.
        for ms in 1..=100u64 {
            health.record_tick(Duration::from_millis(ms));
        }
        health.record_checkpoint(Duration::from_micros(12_340), 3);
        health.record_checkpoint(Duration::from_millis(4), 2);
        health.record_slope_refusal();
        health.record_slope_refusal();
        let report = health.take_report();
        assert_eq!(
            report,
            HealthReport {
                ticks: 100,
                p50_ms: 50.0,
                p99_ms: 99.0,
                max_ms: 100.0,
                over_budget: 50,
                checkpoints: 2,
                checkpoint_max_ms: 12.3,
                checkpoint_conns: 5,
                slope_refusals: 2,
            }
        );
        assert!(report.demands_a_line());

        let empty = health.take_report();
        assert_eq!(empty.ticks, 0);
        assert_eq!(empty.max_ms, 0.0);
        assert_eq!(empty.checkpoints, 0);
        assert_eq!(empty.slope_refusals, 0);
        assert!(!empty.demands_a_line());
    }

    #[test]
    fn a_tick_exactly_on_budget_is_not_an_overrun() {
        let mut health = TickHealth::default();
        health.record_tick(TICK_DT);
        health.record_tick(Duration::from_micros(300));
        let report = health.take_report();
        assert_eq!(report.over_budget, 0);
        assert_eq!(report.p50_ms, 0.3);
        assert_eq!(report.max_ms, 50.0);
        assert!(!report.demands_a_line());
    }

    /// A refused step must reach the log even when its sender has logged out
    /// before the minute ends and no tick overran.
    #[test]
    fn a_refused_step_alone_demands_a_line() {
        let mut health = TickHealth::default();
        health.record_tick(Duration::from_micros(400));
        health.record_slope_refusal();
        let report = health.take_report();
        assert_eq!(report.over_budget, 0);
        assert_eq!(report.slope_refusals, 1);
        assert!(report.demands_a_line());
        assert!(!health.take_report().demands_a_line());
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 99), 7);
        assert_eq!(percentile(&[1, 2, 3, 4], 50), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 99), 4);
    }
}
