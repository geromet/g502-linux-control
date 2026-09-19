//! Pure DPI stepping logic. No I/O here except through `DpiBackend`.

use anyhow::Result;
use std::sync::mpsc::Receiver;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Up,
    Down,
}

/// Move one stage from `current`. `current` need not be a stage (e.g. it was
/// set with `g502ctl dpi set 2000`): Up goes to the next stage above it, Down
/// to the next stage below it. At either end the value stays put.
pub fn step(stages: &[u32], current: u32, step: Step) -> u32 {
    let next = match step {
        Step::Up => stages.iter().copied().find(|&s| s > current),
        Step::Down => stages.iter().copied().rev().find(|&s| s < current),
    };
    next.unwrap_or(current)
}

/// Apply steps one after another. Each step clamps on its own, so
/// Up,Up,Down at the top stage ends one stage below the top, not at the top.
pub fn apply(stages: &[u32], current: u32, steps: &[Step]) -> u32 {
    steps.iter().fold(current, |dpi, &s| step(stages, dpi, s))
}

/// Block for the next press, then take everything else already queued.
/// Presses that piled up while the previous write was in flight become one
/// batch; there is no timer, so an idle press is handled immediately.
/// `None` when the sender is gone.
pub fn next_batch(rx: &Receiver<Step>) -> Option<Vec<Step>> {
    let mut steps = vec![rx.recv().ok()?];
    steps.extend(rx.try_iter());
    Some(steps)
}

/// Where the shared DPI lives. The real one talks to ratbagd; tests use a fake.
pub trait DpiBackend {
    fn get_dpi(&mut self) -> Result<u32>;
    /// Must write every synced profile in a single hardware transaction.
    fn set_dpi(&mut self, dpi: u32) -> Result<()>;
}

/// Read the current DPI, fold all `steps` into it and write once.
/// Returns `Some((from, to))` if a write happened, `None` if nothing changed
/// (already at a boundary, or the steps cancelled out).
pub fn apply_batch(
    backend: &mut impl DpiBackend,
    stages: &[u32],
    steps: &[Step],
) -> Result<Option<(u32, u32)>> {
    let from = backend.get_dpi()?;
    let to = apply(stages, from, steps);
    if to == from {
        return Ok(None);
    }
    backend.set_dpi(to)?;
    Ok(Some((from, to)))
}

#[cfg(test)]
mod tests {
    use super::Step::{Down, Up};
    use super::*;

    const STAGES: [u32; 5] = [1000, 1800, 2400, 3200, 4000];

    struct Fake {
        dpi: u32,
        writes: Vec<u32>,
    }

    impl DpiBackend for Fake {
        fn get_dpi(&mut self) -> Result<u32> {
            Ok(self.dpi)
        }
        fn set_dpi(&mut self, dpi: u32) -> Result<()> {
            self.dpi = dpi;
            self.writes.push(dpi);
            Ok(())
        }
    }

    #[test]
    fn single_steps() {
        assert_eq!(step(&STAGES, 1800, Up), 2400);
        assert_eq!(step(&STAGES, 1800, Down), 1000);
    }

    #[test]
    fn boundaries_clamp() {
        assert_eq!(step(&STAGES, 4000, Up), 4000);
        assert_eq!(step(&STAGES, 1000, Down), 1000);
    }

    #[test]
    fn off_stage_values_snap_to_neighbours() {
        assert_eq!(step(&STAGES, 2000, Up), 2400);
        assert_eq!(step(&STAGES, 2000, Down), 1800);
        assert_eq!(step(&STAGES, 500, Up), 1000);
        assert_eq!(step(&STAGES, 500, Down), 500);
        assert_eq!(step(&STAGES, 5000, Up), 5000);
        assert_eq!(step(&STAGES, 5000, Down), 4000);
    }

    #[test]
    fn rapid_ups_do_not_lose_steps() {
        assert_eq!(apply(&STAGES, 1000, &[Up, Up, Up]), 3200);
    }

    #[test]
    fn bursts_clamp_per_step_not_on_the_sum() {
        // Sum is +1 but the first Up is swallowed by the top boundary.
        assert_eq!(apply(&STAGES, 4000, &[Up, Up, Down]), 3200);
        assert_eq!(apply(&STAGES, 1000, &[Down, Down, Up]), 1800);
    }

    #[test]
    fn long_bursts_stay_in_range() {
        let ups = [Up; 50];
        let downs = [Down; 50];
        assert_eq!(apply(&STAGES, 1800, &ups), 4000);
        assert_eq!(apply(&STAGES, 3200, &downs), 1000);
    }

    #[test]
    fn burst_is_one_write() {
        let mut b = Fake { dpi: 1000, writes: vec![] };
        let r = apply_batch(&mut b, &STAGES, &[Up, Up, Up]).unwrap();
        assert_eq!(r, Some((1000, 3200)));
        assert_eq!(b.writes, vec![3200]);
    }

    #[test]
    fn no_write_when_nothing_changes() {
        let mut b = Fake { dpi: 4000, writes: vec![] };
        assert_eq!(apply_batch(&mut b, &STAGES, &[Up, Up]).unwrap(), None);
        let mut b = Fake { dpi: 1800, writes: vec![] };
        assert_eq!(apply_batch(&mut b, &STAGES, &[Up, Down]).unwrap(), None);
        assert!(b.writes.is_empty());
    }

    #[test]
    fn queued_presses_become_one_batch_and_one_write() {
        let (tx, rx) = std::sync::mpsc::channel();
        for s in [Up, Up, Up] {
            tx.send(s).unwrap();
        }
        let batch = next_batch(&rx).unwrap();
        assert_eq!(batch, vec![Up, Up, Up]);
        let mut b = Fake { dpi: 1000, writes: vec![] };
        apply_batch(&mut b, &STAGES, &batch).unwrap();
        assert_eq!((b.dpi, b.writes.len()), (3200, 1));
    }

    #[test]
    fn idle_press_is_not_delayed_or_merged() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Down).unwrap();
        assert_eq!(next_batch(&rx).unwrap(), vec![Down]);
        drop(tx);
        assert_eq!(next_batch(&rx), None);
    }

    #[test]
    fn sequential_batches_see_each_others_result() {
        let mut b = Fake { dpi: 1000, writes: vec![] };
        apply_batch(&mut b, &STAGES, &[Up]).unwrap();
        apply_batch(&mut b, &STAGES, &[Up, Up]).unwrap();
        assert_eq!(b.dpi, 3200);
    }
}
