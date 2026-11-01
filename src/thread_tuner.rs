use std::collections::HashMap;

/// A probe that added a worker is kept only if the total speed rose by at least this much.
const MIN_GAIN_PERCENT: u64 = 5;
/// A probe that removed a worker is kept only if the total speed fell by at most this much.
const MAX_LOSS_PERCENT: u64 = 5;
/// After a probe fails, the same job is not probed in the same direction for this many ticks.
/// Re-probing later lets the tuner follow changes in the user's connection.
const FAILED_PROBE_COOLDOWN_TICKS: u64 = 10;

#[derive(Debug, Clone, Copy)]
pub struct TunableJob {
    pub id: usize,
    pub worker_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerAdjustment {
    pub job_id: usize,
    pub delta: isize,
}

#[derive(Debug, Clone, Copy)]
struct Probe {
    adjustment: WorkerAdjustment,
    baseline_bps: u64,
}

/// Hill-climbs worker counts against the **total** speed of all downloads. A change is kept
/// only if the total improves (or, for a removal, does not drop), so one torrent cannot grow
/// by taking bandwidth from another, and workers that add nothing are shed.
pub struct ThreadTuner {
    max_workers: usize,
    tick: u64,
    probe_direction: isize,
    pending_probe: Option<Probe>,
    blocked_until_tick: HashMap<(usize, isize), u64>,
}

impl ThreadTuner {
    pub fn new(max_workers: usize) -> Self {
        Self {
            max_workers,
            tick: 0,
            probe_direction: 1,
            pending_probe: None,
            blocked_until_tick: HashMap::new(),
        }
    }

    /// Called once per tuning interval with the current total speed of every download.
    /// Returns the worker change to apply, if any. At most one change is made per tick so
    /// each one can be measured on its own.
    pub fn step(
        &mut self,
        total_speed_bps: u64,
        tunable_jobs: &[TunableJob],
    ) -> Option<WorkerAdjustment> {
        self.tick += 1;
        let current_tick = self.tick;
        self.blocked_until_tick
            .retain(|_, until_tick| *until_tick > current_tick);

        if let Some(probe) = self.pending_probe.take() {
            if let Some(revert) = self.judge_probe(probe, total_speed_bps, tunable_jobs) {
                return Some(revert);
            }
        }

        // With nothing flowing there is no signal to compare against.
        if total_speed_bps == 0 {
            return None;
        }
        self.start_probe(total_speed_bps, tunable_jobs)
    }

    /// Returns the undo adjustment when the probe did not pay off.
    fn judge_probe(
        &mut self,
        probe: Probe,
        total_speed_bps: u64,
        tunable_jobs: &[TunableJob],
    ) -> Option<WorkerAdjustment> {
        let WorkerAdjustment { job_id, delta } = probe.adjustment;
        if !tunable_jobs.iter().any(|job| job.id == job_id) {
            return None;
        }

        let required_percent = if delta > 0 {
            100 + MIN_GAIN_PERCENT
        } else {
            100 - MAX_LOSS_PERCENT
        };
        if total_speed_bps * 100 >= probe.baseline_bps * required_percent {
            return None;
        }

        self.blocked_until_tick
            .insert((job_id, delta), self.tick + FAILED_PROBE_COOLDOWN_TICKS);
        self.probe_direction = -delta;
        Some(WorkerAdjustment {
            job_id,
            delta: -delta,
        })
    }

    fn start_probe(
        &mut self,
        total_speed_bps: u64,
        tunable_jobs: &[TunableJob],
    ) -> Option<WorkerAdjustment> {
        let adjustment = match self.pick_adjustment(self.probe_direction, tunable_jobs) {
            Some(adjustment) => adjustment,
            None => {
                self.probe_direction = -self.probe_direction;
                self.pick_adjustment(self.probe_direction, tunable_jobs)?
            }
        };
        self.pending_probe = Some(Probe {
            adjustment,
            baseline_bps: total_speed_bps,
        });
        Some(adjustment)
    }

    fn pick_adjustment(
        &self,
        direction: isize,
        tunable_jobs: &[TunableJob],
    ) -> Option<WorkerAdjustment> {
        let candidates = tunable_jobs.iter().filter(|job| {
            let has_room = if direction > 0 {
                job.worker_count < self.max_workers
            } else {
                job.worker_count > 1
            };
            has_room && !self.blocked_until_tick.contains_key(&(job.id, direction))
        });
        // Growing the smallest pool and shrinking the largest keeps workers spread fairly.
        let chosen = if direction > 0 {
            candidates.min_by_key(|job| job.worker_count)
        } else {
            candidates.max_by_key(|job| job.worker_count)
        }?;
        Some(WorkerAdjustment {
            job_id: chosen.id,
            delta: direction,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PER_CONNECTION_BPS: u64 = 256 * 1024;
    const MIB_PER_SEC: u64 = 1024 * 1024;

    /// Each connection is latency-bound to `PER_CONNECTION_BPS`, and all of them share one
    /// internet link of `link_capacity_bps`.
    fn simulated_total_bps(worker_counts: &[usize], link_capacity_bps: u64) -> u64 {
        let total_workers: usize = worker_counts.iter().sum();
        (total_workers as u64 * PER_CONNECTION_BPS).min(link_capacity_bps)
    }

    /// Runs the tuner for `ticks` intervals and returns the total worker count after each tick.
    fn run_ticks(
        tuner: &mut ThreadTuner,
        worker_counts: &mut [usize],
        link_capacity_bps: u64,
        ticks: usize,
    ) -> Vec<usize> {
        let mut total_workers_per_tick = Vec::with_capacity(ticks);
        for _ in 0..ticks {
            let total_speed_bps = simulated_total_bps(worker_counts, link_capacity_bps);
            let tunable_jobs: Vec<TunableJob> = worker_counts
                .iter()
                .enumerate()
                .map(|(id, &worker_count)| TunableJob { id, worker_count })
                .collect();
            if let Some(adjustment) = tuner.step(total_speed_bps, &tunable_jobs) {
                let worker_count = &mut worker_counts[adjustment.job_id];
                *worker_count = worker_count.saturating_add_signed(adjustment.delta);
            }
            total_workers_per_tick.push(worker_counts.iter().sum());
        }
        total_workers_per_tick
    }

    /// The settled worker count: the most frequent total over the given ticks.
    fn most_common(totals: &[usize]) -> usize {
        let mut occurrences: HashMap<usize, usize> = HashMap::new();
        for &total in totals {
            *occurrences.entry(total).or_default() += 1;
        }
        occurrences
            .into_iter()
            .max_by_key(|&(_, count)| count)
            .map(|(total, _)| total)
            .unwrap()
    }

    #[test]
    fn test_single_download_climbs_to_link_capacity_and_stops() {
        let mut tuner = ThreadTuner::new(16);
        let mut worker_counts = [1];
        let totals = run_ticks(&mut tuner, &mut worker_counts, MIB_PER_SEC, 60);

        let settled = &totals[30..];
        assert_eq!(most_common(settled), 4);
        assert!(settled.iter().all(|&total| (3..=5).contains(&total)));
    }

    #[test]
    fn test_over_provisioned_download_sheds_useless_workers() {
        let mut tuner = ThreadTuner::new(16);
        let mut worker_counts = [12];
        let totals = run_ticks(&mut tuner, &mut worker_counts, MIB_PER_SEC, 60);

        assert_eq!(most_common(&totals[30..]), 4);
    }

    #[test]
    fn test_parallel_downloads_do_not_grow_past_shared_link() {
        let mut tuner = ThreadTuner::new(16);
        let mut worker_counts = [8, 1];
        let totals = run_ticks(&mut tuner, &mut worker_counts, MIB_PER_SEC, 80);

        assert_eq!(most_common(&totals[40..]), 4);
        assert!(worker_counts.iter().all(|&count| count >= 1));
    }

    #[test]
    fn test_follows_connection_speedup() {
        let mut tuner = ThreadTuner::new(16);
        let mut worker_counts = [1];
        run_ticks(&mut tuner, &mut worker_counts, MIB_PER_SEC, 40);
        let totals = run_ticks(&mut tuner, &mut worker_counts, 2 * MIB_PER_SEC, 80);

        assert_eq!(most_common(&totals[40..]), 8);
    }

    #[test]
    fn test_never_exceeds_max_workers_and_ignores_zero_speed() {
        let mut tuner = ThreadTuner::new(16);
        let mut worker_counts = [1];
        let totals = run_ticks(&mut tuner, &mut worker_counts, 100 * MIB_PER_SEC, 80);
        assert!(totals.iter().all(|&total| total <= 16));
        assert_eq!(most_common(&totals[40..]), 16);

        let idle_job = [TunableJob {
            id: 0,
            worker_count: 4,
        }];
        assert_eq!(ThreadTuner::new(16).step(0, &idle_job), None);
    }
}
