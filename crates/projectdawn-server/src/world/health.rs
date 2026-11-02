//! The world loop's own health: how long each tick's work takes and how long
//! the 60 s checkpoint holds the loop, reported once a minute as the
//! `world health` log line beside `enemy position fan` and `player link`.
//!
//! A tick has `TICK_DT` (50 ms) to do its work. The loop awaits the database
//! inline (logins, the checkpoint, the per-tick quest and drop saves), so a
//! slow disk or a crowded world shows up here first, as a tick that ran past
//! its budget, long before players call it lag. The numbers are for the
//! operator: nothing in the game reads them.

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
}

impl HealthReport {
    /// A window worth a line even with nobody in the world: a tick ran past
    /// its budget.
    pub fn has_overrun(&self) -> bool {
        self.over_budget > 0
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

    /// The window's report, and a fresh window.
    pub fn take_report(&mut self) -> HealthReport {
        let mut ticks = std::mem::take(&mut self.tick_us);
        let checkpoints = std::mem::take(&mut self.checkpoint_us);
        let checkpoint_conns = std::mem::take(&mut self.checkpoint_conns);
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
            }
        );
        assert!(report.has_overrun());

        let empty = health.take_report();
        assert_eq!(empty.ticks, 0);
        assert_eq!(empty.max_ms, 0.0);
        assert_eq!(empty.checkpoints, 0);
        assert!(!empty.has_overrun());
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
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 99), 7);
        assert_eq!(percentile(&[1, 2, 3, 4], 50), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 99), 4);
    }
}
