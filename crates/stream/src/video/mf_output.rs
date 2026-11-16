//! Media Foundation output collection and retained-picture delay detection.
use anyhow::{ensure, Result};
use std::time::{Duration, Instant};

const PICTURE_WAIT: Duration = Duration::from_millis(50);
const RETAINED_DELAY: &str = "two consecutive inputs returned only the previous picture";

pub(super) enum Output<T> {
    Picture(i64, T),
    NeedInput,
    StreamChange,
    Events,
}

pub(super) struct OutputBudget {
    pub(super) deadline: Instant,
    pub(super) calls: usize,
    pub(super) waits: usize,
    pub(super) stream_changes: usize,
    pub(super) events: usize,
}
impl OutputBudget {
    pub(super) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            calls: 0,
            waits: 0,
            stream_changes: 0,
            events: 0,
        }
    }
    fn check(&mut self, now: Instant) -> Result<()> {
        ensure!(
            now < self.deadline,
            "decoder output deadline exceeded after {} calls ({} stream changes, {} event-only outputs, {} waits)",
            self.calls, self.stream_changes, self.events, self.waits
        );
        ensure!(
            self.calls < 2048,
            "decoder output exceeded 2048 calls ({} stream changes, {} event-only outputs, {} waits)",
            self.stream_changes, self.events, self.waits
        );
        ensure!(
            self.stream_changes + self.events < 64,
            "decoder output exceeded 64 format/event transitions ({} stream changes, {} event-only outputs)",
            self.stream_changes, self.events
        );
        self.calls += 1;
        Ok(())
    }
}

pub(super) struct PictureWait {
    cold: bool,
    picture_seen: bool,
    pub(super) pending: Option<i64>,
    delayed_inputs: u8,
    pub(super) disabled_reason: Option<&'static str>,
    warned: bool,
}
impl Default for PictureWait {
    fn default() -> Self {
        Self {
            cold: true,
            picture_seen: false,
            pending: None,
            delayed_inputs: 0,
            disabled_reason: None,
            warned: false,
        }
    }
}
impl PictureWait {
    fn reconfigure(&mut self) -> bool {
        // The initial output type may become available only on another input.
        // Keep that startup within its original cold wait until a picture exists.
        if !self.picture_seen && self.pending.is_some() {
            return false;
        }
        let warned = self.warned;
        *self = Self::default();
        self.warned = warned;
        true
    }
    fn before_input(&mut self, time: i64) {
        if self.pending == Some(time) {
            // It completed without another input, so this was a slow picture.
            self.pending = None;
            self.delayed_inputs = 0;
            self.cold = false;
        }
    }
}

pub(super) trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}
#[cfg(windows)]
pub(super) struct SystemClock;
#[cfg(windows)]
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Keep every original output identity, including partial batches on error.
/// After a missed picture, probe subsequent inputs without adding another wait.
pub(super) fn drain_output<T>(
    pictures: &mut Vec<(i64, T)>,
    budget: &mut OutputBudget,
    expected: Option<i64>,
    wait: &mut PictureWait,
    on_disabled: impl FnOnce(&'static str),
    mut next: impl FnMut(Instant) -> Result<Output<T>>,
    clock: &impl Clock,
) -> Result<()> {
    let mut previous = wait.pending;
    let first_output = pictures.len();
    let contains_current = |pictures: &[(i64, T)]| {
        expected.is_some_and(|time| pictures.iter().any(|picture| picture.0 == time))
    };
    let mut waiting = expected.is_some()
        && wait.disabled_reason.is_none()
        && wait.pending.is_none()
        && !contains_current(pictures);
    // Cold output types, allocators and MFT threads share the existing access
    // unit deadline. The caller has already reserved time for the worker reply.
    let mut wait_deadline = if wait.cold {
        budget.deadline
    } else {
        budget.deadline.min(clock.now() + PICTURE_WAIT)
    };
    loop {
        let now = clock.now();
        if waiting && now >= wait_deadline {
            break;
        }
        budget.check(now)?;
        let deadline = if waiting {
            wait_deadline
        } else {
            budget.deadline
        };
        match next(deadline)? {
            Output::Picture(time, picture) => {
                ensure!(
                    pictures.len() < 64,
                    "decoder produced too many outputs without needing input"
                );
                pictures.push((time, picture));
                wait.picture_seen = true;
                if expected == Some(time) {
                    waiting = false;
                }
                if expected.is_none() {
                    wait.before_input(time);
                }
            }
            Output::StreamChange => {
                budget.stream_changes += 1;
                if wait.reconfigure() {
                    previous = None;
                    waiting = expected.is_some() && !contains_current(pictures);
                    wait_deadline = budget.deadline;
                }
            }
            Output::Events => budget.events += 1,
            Output::NeedInput => {
                if !waiting {
                    break;
                }
                budget.waits += 1;
                clock.sleep(
                    Duration::from_millis(1)
                        .min(wait_deadline.saturating_duration_since(clock.now())),
                );
            }
        }
    }
    if let Some(time) = expected {
        wait.cold = false;
        if contains_current(pictures) {
            wait.pending = None;
            wait.delayed_inputs = 0;
        } else {
            let returned_previous = previous.is_some_and(|previous| {
                pictures[first_output..]
                    .iter()
                    .any(|picture| picture.0 == previous)
            });
            wait.delayed_inputs = if returned_previous {
                wait.delayed_inputs.saturating_add(1)
            } else {
                0
            };
            wait.pending = Some(time);
            if wait.delayed_inputs >= 2 && wait.disabled_reason.is_none() {
                wait.disabled_reason = Some(RETAINED_DELAY);
                if !wait.warned {
                    wait.warned = true;
                    on_disabled(RETAINED_DELAY);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    const DECODE: Duration = Duration::from_secs(1);

    struct FakeClock(Cell<Instant>);
    impl Default for FakeClock {
        fn default() -> Self {
            Self(Cell::new(Instant::now()))
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.0.get()
        }
        fn sleep(&self, duration: Duration) {
            self.0.set(self.now() + duration);
        }
    }
    #[test]
    fn one_drain_returns_every_ready_picture_in_order() -> Result<()> {
        let clock = FakeClock::default();
        let mut wait_enabled = PictureWait::default();
        // Reproduce the missing Linux capture identities 2..5: all five
        // pictures become ready before the next ProcessInput. The source
        // is deterministic and uses the collector State::drain calls.
        let mut ready: std::collections::VecDeque<(i64, Vec<u8>)> = (2..=6)
            .map(|identity| (identity * 166667, vec![identity as u8; 6]))
            .collect();
        let mut pictures = Vec::new();
        let mut budget = OutputBudget::new(clock.now() + DECODE);
        drain_output(
            &mut pictures,
            &mut budget,
            None,
            &mut wait_enabled,
            |_| panic!("unexpected wait fallback"),
            |_deadline| {
                Ok(match ready.pop_front() {
                    Some((time, picture)) => Output::Picture(time, picture),
                    None => Output::NeedInput,
                })
            },
            &clock,
        )?;
        assert!(ready.is_empty());
        assert_eq!(
            pictures,
            (2..=6)
                .map(|i| (i * 166667, vec![i as u8; 6]))
                .collect::<Vec<_>>()
        );
        // A later drain leaves the returned batch intact.
        drain_output(
            &mut pictures,
            &mut budget,
            None,
            &mut wait_enabled,
            |_| panic!("unexpected wait fallback"),
            |_deadline| Ok(Output::NeedInput),
            &clock,
        )?;
        assert_eq!(pictures.len(), 5);
        Ok(())
    }
    #[test]
    fn a_partial_drain_keeps_output_across_an_error_and_retry() -> Result<()> {
        let clock = FakeClock::default();
        let mut wait_enabled = PictureWait::default();
        let mut pictures = Vec::new();
        let mut outputs = [
            Ok(Output::Picture(2, 2)),
            Ok(Output::Picture(3, 3)),
            Err(anyhow::anyhow!("MFT failure")),
        ]
        .into_iter();
        let mut budget = OutputBudget::new(clock.now() + DECODE);
        assert!(drain_output(
            &mut pictures,
            &mut budget,
            None,
            &mut wait_enabled,
            |_| panic!("unexpected wait fallback"),
            |_deadline| outputs.next().unwrap(),
            &clock
        )
        .is_err());
        assert_eq!(pictures, [(2, 2), (3, 3)]);
        let mut ready = [4, 5, 6].into_iter();
        drain_output(
            &mut pictures,
            &mut budget,
            None,
            &mut wait_enabled,
            |_| panic!("unexpected wait fallback"),
            |_deadline| {
                Ok(match ready.next() {
                    Some(picture) => Output::Picture(picture, picture),
                    None => Output::NeedInput,
                })
            },
            &clock,
        )?;
        assert_eq!(pictures, [(2, 2), (3, 3), (4, 4), (5, 5), (6, 6)]);
        Ok(())
    }
    #[test]
    fn late_picture_after_stream_change_arrives_before_another_input() -> Result<()> {
        let clock = FakeClock::default();
        let mut wait_enabled = PictureWait::default();
        let mut outputs = [
            Output::NeedInput,
            Output::StreamChange,
            Output::NeedInput,
            Output::Events,
            Output::NeedInput,
            Output::Picture(0, vec![32; 6]),
            Output::NeedInput,
        ]
        .into_iter();
        let mut budget = OutputBudget::new(clock.now() + DECODE);
        let mut pictures = Vec::new();
        let submitted_inputs = std::cell::Cell::new(1);
        drain_output(
            &mut pictures,
            &mut budget,
            Some(0),
            &mut wait_enabled,
            |_| panic!("unexpected wait fallback"),
            |_deadline| {
                assert_eq!(
                    submitted_inputs.get(),
                    1,
                    "output must precede the next input"
                );
                Ok(outputs.next().unwrap())
            },
            &clock,
        )?;
        assert_eq!(pictures, [(0, vec![32; 6])]);
        assert_eq!(
            (
                budget.calls,
                budget.stream_changes,
                budget.events,
                budget.waits
            ),
            (7, 1, 1, 3)
        );
        assert!(
            wait_enabled.disabled_reason.is_none(),
            "a picture inside the cap keeps polling enabled"
        );
        submitted_inputs.set(2);
        let mut second = [
            Output::NeedInput,
            Output::Picture(1, vec![40; 6]),
            Output::NeedInput,
        ]
        .into_iter();
        drain_output(
            &mut pictures,
            &mut budget,
            Some(1),
            &mut wait_enabled,
            |_| panic!("a transient delay must not switch off polling"),
            |_| Ok(second.next().unwrap()),
            &clock,
        )?;
        assert!(wait_enabled.disabled_reason.is_none());
        assert_eq!(pictures, [(0, vec![32; 6]), (1, vec![40; 6])]);
        Ok(())
    }
    #[test]
    fn repeated_stream_changes_and_event_only_outputs_share_one_call_limit() {
        let clock = FakeClock::default();
        let mut wait_enabled = PictureWait::default();
        let mut budget = OutputBudget::new(clock.now() + DECODE);
        let mut pictures: Vec<(i64, ())> = Vec::new();
        let mut calls = 0;
        let error = drain_output(
            &mut pictures,
            &mut budget,
            None,
            &mut wait_enabled,
            |_| panic!("unexpected wait fallback"),
            |_deadline| {
                calls += 1;
                Ok(if calls % 2 == 0 {
                    Output::StreamChange
                } else {
                    Output::Events
                })
            },
            &clock,
        )
        .unwrap_err();
        assert_eq!(calls, 64);
        assert_eq!((budget.stream_changes, budget.events), (32, 32));
        assert!(
            error
                .to_string()
                .contains("exceeded 64 format/event transitions"),
            "{error:#}"
        );
    }
    #[test]
    fn output_call_and_picture_counts_are_bounded() {
        let clock = FakeClock::default();
        let mut wait_enabled = PictureWait::default();
        let mut pictures: Vec<(i64, ())> = Vec::new();
        let mut budget = OutputBudget::new(clock.now() + DECODE);
        budget.calls = 2048;
        let error = drain_output(
            &mut pictures,
            &mut budget,
            None,
            &mut wait_enabled,
            |_| panic!("unexpected wait fallback"),
            |_deadline| panic!("exhausted call budget must stop before another call"),
            &clock,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("exceeded 2048 calls"),
            "{error:#}"
        );
        let mut budget = OutputBudget::new(clock.now() + DECODE);
        let error = drain_output(
            &mut pictures,
            &mut budget,
            None,
            &mut wait_enabled,
            |_| panic!("unexpected wait fallback"),
            |_deadline| Ok(Output::Picture(0, ())),
            &clock,
        )
        .unwrap_err();
        assert!(error.to_string().contains("too many outputs"), "{error:#}");
        assert_eq!(pictures.len(), 64);
    }
    #[test]
    fn output_deadline_stops_calls_without_waiting() {
        let clock = FakeClock::default();
        let mut pictures: Vec<(i64, ())> = Vec::new();
        let mut budget = OutputBudget::new(clock.now());
        let error = drain_output(
            &mut pictures,
            &mut budget,
            None,
            &mut PictureWait::default(),
            |_| panic!("no picture was expected"),
            |_| panic!("expired budget must not call the transform"),
            &clock,
        )
        .unwrap_err();
        assert!(error.to_string().contains("deadline exceeded"), "{error:#}");
    }
    #[test]
    fn an_unqualified_or_reordered_stream_can_still_need_more_input() -> Result<()> {
        let clock = FakeClock::default();
        let mut wait_enabled = PictureWait::default();
        let mut pictures: Vec<(i64, ())> = Vec::new();
        let mut budget = OutputBudget::new(clock.now() + DECODE);
        drain_output(
            &mut pictures,
            &mut budget,
            None,
            &mut wait_enabled,
            |_| panic!("unexpected wait fallback"),
            |_deadline| Ok(Output::NeedInput),
            &clock,
        )?;
        assert_eq!((budget.calls, budget.waits), (1, 0));
        Ok(())
    }
    fn submit(
        wait: &mut PictureWait,
        clock: &FakeClock,
        time: i64,
        warnings: &Cell<usize>,
        next: impl FnMut(Instant) -> Result<Output<i64>>,
    ) -> Result<Vec<(i64, i64)>> {
        let mut pictures = Vec::new();
        let mut budget = OutputBudget::new(clock.now() + DECODE - Duration::from_millis(20));
        drain_output(
            &mut pictures,
            &mut budget,
            Some(time),
            wait,
            |_| warnings.set(warnings.get() + 1),
            next,
            clock,
        )?;
        Ok(pictures)
    }

    fn prompt(
        wait: &mut PictureWait,
        clock: &FakeClock,
        time: i64,
        warnings: &Cell<usize>,
    ) -> Result<Vec<(i64, i64)>> {
        let mut picture = Some(time);
        submit(wait, clock, time, warnings, |_| {
            Ok(picture
                .take()
                .map_or(Output::NeedInput, |time| Output::Picture(time, time)))
        })
    }

    #[test]
    fn slow_cold_start_keeps_polling_and_delivers_each_picture_on_its_input() -> Result<()> {
        let clock = FakeClock::default();
        let mut wait = PictureWait::default();
        let warnings = Cell::new(0);
        let started = clock.now();
        let mut picture = Some(0);
        let first = submit(&mut wait, &clock, 0, &warnings, |deadline| {
            assert!(deadline > started + Duration::from_millis(300));
            Ok(if clock.now() < started + Duration::from_millis(300) {
                Output::NeedInput
            } else {
                picture
                    .take()
                    .map_or(Output::NeedInput, |time| Output::Picture(time, time))
            })
        })?;
        assert_eq!(first, [(0, 0)]);
        assert_eq!(clock.now() - started, Duration::from_millis(300));
        for time in 1..4 {
            assert_eq!(prompt(&mut wait, &clock, time, &warnings)?, [(time, time)]);
        }
        assert!(wait.disabled_reason.is_none());
        assert_eq!(warnings.get(), 0);
        Ok(())
    }

    #[test]
    fn permanent_picture_delay_pays_one_cold_wait_and_preserves_every_identity() -> Result<()> {
        let clock = FakeClock::default();
        let mut wait = PictureWait::default();
        let warnings = Cell::new(0);
        let started = clock.now();
        let mut pending = None;
        let mut published = Vec::new();
        for time in 0..8 {
            let mut ready = pending.replace(time);
            let pictures = submit(&mut wait, &clock, time, &warnings, |_| {
                Ok(ready
                    .take()
                    .map_or(Output::NeedInput, |time| Output::Picture(time, time)))
            })?;
            if time == 0 {
                assert!(pictures.is_empty());
            } else {
                assert_eq!(pictures, [(time - 1, time - 1)]);
            }
            assert_eq!(clock.now() - started, Duration::from_millis(980));
            assert_eq!(warnings.get(), usize::from(time >= 2));
            assert_eq!(wait.disabled_reason.is_some(), time >= 2);
            published.extend(pictures);
        }
        // One more input releases the last tested picture. Its own held
        // picture remains pending and is discarded when the decoder stops.
        let mut ready = pending.replace(8);
        published.extend(submit(&mut wait, &clock, 8, &warnings, |_| {
            Ok(ready
                .take()
                .map_or(Output::NeedInput, |time| Output::Picture(time, time)))
        })?);
        assert_eq!(pending, Some(8));
        assert_eq!(
            published,
            (0..8).map(|time| (time, time)).collect::<Vec<_>>()
        );
        assert_eq!(warnings.get(), 1);
        assert_eq!(clock.now() - started, Duration::from_millis(980));
        Ok(())
    }

    #[test]
    fn a_single_steady_state_hiccup_keeps_polling_enabled() -> Result<()> {
        let clock = FakeClock::default();
        let mut wait = PictureWait::default();
        let warnings = Cell::new(0);
        assert_eq!(prompt(&mut wait, &clock, 0, &warnings)?, [(0, 0)]);
        let started = clock.now();
        assert!(submit(&mut wait, &clock, 1, &warnings, |_| Ok(Output::NeedInput))?.is_empty());
        assert_eq!(clock.now() - started, PICTURE_WAIT);
        assert!(wait.disabled_reason.is_none());
        assert_eq!(warnings.get(), 0);
        // This one picture finishes after 80 ms, before another input. Its
        // late identity is retained and polling resumes for the next picture.
        clock.sleep(Duration::from_millis(30));
        let mut ready = Some(1);
        let mut pictures = Vec::new();
        let mut budget = OutputBudget::new(clock.now() + DECODE);
        drain_output(
            &mut pictures,
            &mut budget,
            None,
            &mut wait,
            |_| warnings.set(warnings.get() + 1),
            |_| {
                Ok(ready
                    .take()
                    .map_or(Output::NeedInput, |time| Output::Picture(time, time)))
            },
            &clock,
        )?;
        assert_eq!(pictures, [(1, 1)]);
        assert_eq!(prompt(&mut wait, &clock, 2, &warnings)?, [(2, 2)]);
        assert!(wait.disabled_reason.is_none());
        assert_eq!(warnings.get(), 0);
        // An earlier picture and the current one in the same post-input batch
        // also count as recovery, without switching polling off.
        assert!(submit(&mut wait, &clock, 3, &warnings, |_| Ok(Output::NeedInput))?.is_empty());
        let mut ready = [3, 4].into_iter();
        let pictures = submit(&mut wait, &clock, 4, &warnings, |_| {
            Ok(ready
                .next()
                .map_or(Output::NeedInput, |time| Output::Picture(time, time)))
        })?;
        assert_eq!(pictures, [(3, 3), (4, 4)]);
        assert!(wait.disabled_reason.is_none());
        assert_eq!(warnings.get(), 0);
        Ok(())
    }

    #[test]
    fn a_decoder_that_starts_retaining_pictures_pays_one_short_wait() -> Result<()> {
        let clock = FakeClock::default();
        let mut wait = PictureWait::default();
        let warnings = Cell::new(0);
        assert_eq!(prompt(&mut wait, &clock, 0, &warnings)?, [(0, 0)]);
        let started = clock.now();
        let mut pending = None;
        let mut published = Vec::new();
        for time in 1..8 {
            let mut ready = pending.replace(time);
            let pictures = submit(&mut wait, &clock, time, &warnings, |_| {
                Ok(ready
                    .take()
                    .map_or(Output::NeedInput, |time| Output::Picture(time, time)))
            })?;
            if time == 1 {
                assert!(pictures.is_empty());
            } else {
                assert_eq!(pictures, [(time - 1, time - 1)]);
            }
            assert_eq!(clock.now() - started, PICTURE_WAIT);
            assert_eq!(wait.disabled_reason.is_some(), time >= 3);
            assert_eq!(warnings.get(), usize::from(time >= 3));
            published.extend(pictures);
        }
        assert_eq!(
            published,
            (1..7).map(|time| (time, time)).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn stream_change_rearms_cold_output_setup_within_the_worker_deadline() -> Result<()> {
        let clock = FakeClock::default();
        let mut wait = PictureWait::default();
        let warnings = Cell::new(0);
        assert_eq!(prompt(&mut wait, &clock, 0, &warnings)?, [(0, 0)]);
        let started = clock.now();
        let mut change = true;
        let mut picture = Some(1);
        let pictures = submit(&mut wait, &clock, 1, &warnings, |deadline| {
            if change {
                change = false;
                return Ok(Output::StreamChange);
            }
            assert!(deadline > started + Duration::from_millis(300));
            Ok(if clock.now() < started + Duration::from_millis(300) {
                Output::NeedInput
            } else {
                picture
                    .take()
                    .map_or(Output::NeedInput, |time| Output::Picture(time, time))
            })
        })?;
        assert_eq!(pictures, [(1, 1)]);
        assert_eq!(clock.now() - started, Duration::from_millis(300));
        assert!(wait.disabled_reason.is_none());
        assert_eq!(warnings.get(), 0);
        Ok(())
    }

    #[test]
    fn pending_output_type_setup_uses_the_cold_budget_without_disabling_polling() -> Result<()> {
        let clock = FakeClock::default();
        let mut wait = PictureWait::default();
        let warnings = Cell::new(0);
        let started = clock.now();
        let pictures = submit(&mut wait, &clock, 0, &warnings, |deadline| {
            assert_eq!(deadline, started + DECODE - Duration::from_millis(20));
            Ok(Output::NeedInput)
        })?;
        assert!(pictures.is_empty());
        assert_eq!(clock.now() - started, Duration::from_millis(980));
        assert!(wait.disabled_reason.is_none());
        assert_eq!(warnings.get(), 0);
        // With that identity unresolved, another input probes without waiting.
        assert!(submit(&mut wait, &clock, 1, &warnings, |_| Ok(Output::NeedInput))?.is_empty());
        assert_eq!(clock.now() - started, Duration::from_millis(980));
        Ok(())
    }
    #[test]
    fn delayed_initial_type_negotiation_does_not_spend_a_second_cold_wait() -> Result<()> {
        let clock = FakeClock::default();
        let mut wait = PictureWait::default();
        let warnings = Cell::new(0);
        let started = clock.now();
        assert!(submit(&mut wait, &clock, 0, &warnings, |_| Ok(Output::NeedInput))?.is_empty());
        let mut outputs = [
            Output::StreamChange,
            Output::Picture(0, 0),
            Output::NeedInput,
        ]
        .into_iter();
        assert_eq!(
            submit(&mut wait, &clock, 1, &warnings, |_| Ok(outputs
                .next()
                .unwrap_or(Output::NeedInput)))?,
            [(0, 0)]
        );
        assert_eq!(clock.now() - started, Duration::from_millis(980));
        assert!(wait.disabled_reason.is_none());
        assert_eq!(warnings.get(), 0);
        let mut outputs = [Output::Picture(1, 1), Output::NeedInput].into_iter();
        assert_eq!(
            submit(&mut wait, &clock, 2, &warnings, |_| Ok(outputs
                .next()
                .unwrap_or(Output::NeedInput)))?,
            [(1, 1)]
        );
        assert_eq!(clock.now() - started, Duration::from_millis(980));
        assert_eq!(wait.disabled_reason, Some(RETAINED_DELAY));
        assert_eq!(warnings.get(), 1);
        Ok(())
    }
}
