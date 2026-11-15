//! Platform-independent state for the Windows hook's bounded delivery lease.

#[cfg(any(windows, test))]
use super::Key;
use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Instant,
};

pub const LEASE_MS: u64 = 250;
pub const HEARTBEAT_MS: u64 = LEASE_MS / 3;
#[cfg(any(windows, test))]
pub const BARRIER_MS: u64 = 8;
#[cfg(any(windows, test))]
pub const QUEUE_SIZE: usize = 256;

pub struct Lease {
    start: Instant,
    deadline: AtomicU64,
    pub failed: AtomicBool,
}

impl Default for Lease {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            deadline: AtomicU64::new(LEASE_MS),
            failed: AtomicBool::new(false),
        }
    }
}

impl Lease {
    pub fn renew(&self) {
        self.renew_at(self.elapsed());
    }

    pub fn renew_at(&self, now: u64) {
        self.deadline.store(now + LEASE_MS, Ordering::Release);
    }

    pub fn elapsed(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    pub fn active(&self, now: u64) -> bool {
        !self.failed.load(Ordering::Acquire) && now < self.deadline.load(Ordering::Acquire)
    }

    #[cfg(any(windows, test))]
    pub fn fail(&self) {
        self.failed.store(true, Ordering::Release);
    }
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    pub key: Key,
    pub alt: bool,
    pub time: u32,
    pub altgr: bool,
    pub captured: bool,
    pub queued_at: u64,
}

#[cfg(any(windows, test))]
pub struct Capture {
    consumed: [bool; 256],
    pub altgr: bool,
}

#[cfg(any(windows, test))]
impl Capture {
    pub fn new(altgr: bool) -> Self {
        Self {
            consumed: [false; 256],
            altgr,
        }
    }

    pub fn consume(&mut self, event: Event, deliver: impl FnOnce(Event) -> bool) -> bool {
        let key = event.key;
        let owned_down = self
            .consumed
            .get(usize::from(key.vk))
            .copied()
            .unwrap_or(false);
        if !key.down && matches!(key.vk, 0x5b | 0x5c) && !owned_down {
            return false;
        }
        let swallow = if key.down {
            matches!(key.vk, 0x5b | 0x5c) || (event.alt && matches!(key.vk, 0x09 | 0x20 | 0x73))
        } else {
            owned_down
        };
        // Ordinary events are ordering barriers, translated by winit before
        // later hook modifiers can be published. Every event has bounded delivery.
        if !deliver(Event {
            captured: swallow,
            ..event
        }) {
            return false;
        }
        if let Some(consumed) = self.consumed.get_mut(usize::from(key.vk)) {
            *consumed = key.down && swallow;
        }
        swallow
    }

    pub fn fake_control(altgr: bool, control: Event, next: Event) -> bool {
        altgr
            && next.key.vk == 0xa5
            && control.key.down == next.key.down
            && control.time == next.time
    }
}

/// Defer an AltGr-layout Control until winit identifies a real Control event
/// or the next hook event disambiguates it. Hook events can precede posted
/// winit events and can arrive on opposite sides of a frame boundary.
#[cfg(any(windows, test))]
pub fn take_ready(
    pending: &mut std::collections::VecDeque<Event>,
    native: Option<Key>,
    now: u64,
) -> Vec<Event> {
    let mut events = Vec::new();
    while let Some(event) = pending.front().copied() {
        if matches!(event.key.vk, 0xa2 | 0xa3) && event.altgr {
            match pending.get(1) {
                Some(next) if Capture::fake_control(true, event, *next) => {
                    pending.pop_front();
                    continue;
                }
                None if native != Some(event.key) => {
                    if now.saturating_sub(event.queued_at) < BARRIER_MS {
                        break;
                    }
                    // An unconfirmed lone Control can be an AltGr synthetic event.
                    pending.pop_front();
                    continue;
                }
                _ => {}
            }
        }
        if !event.captured && !matches!(event.key.vk, 0xa0..=0xa5) {
            if native == Some(event.key) {
                pending.pop_front();
                break;
            }
            if now.saturating_sub(event.queued_at) < BARRIER_MS {
                break;
            }
            pending.pop_front();
            continue;
        }
        pending.pop_front();
        events.push(event);
    }
    events
}

#[cfg(any(windows, test))]
pub fn deliver(tx: &std::sync::mpsc::SyncSender<Event>, lease: &Lease, event: Event) -> bool {
    if tx.try_send(event).is_ok() {
        true
    } else {
        lease.fail();
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(vk: u16, down: bool) -> Event {
        Event {
            key: Key { vk, down },
            alt: true,
            time: 1,
            altgr: false,
            captured: matches!(vk, 0x5b | 0x5c),
            queued_at: 0,
        }
    }

    #[test]
    fn queue_handoff_is_immediate_and_full_or_disconnected_queues_fail_open() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let lease = Lease::default();
        let key = event(0x5b, true);
        assert!(deliver(&tx, &lease, key));
        assert_eq!(rx.try_recv(), Ok(key));
        assert!(deliver(&tx, &lease, key));
        assert!(!deliver(&tx, &lease, key));
        assert!(!lease.active(0));
        let lease = Lease::default();
        drop(rx);
        assert!(!deliver(&tx, &lease, key));
        assert!(!lease.active(0));
    }

    #[test]
    fn missing_barrier_expires_in_eight_ms_without_releasing_held_input() {
        use crate::input::{Recorder, Sent, SharedHeld};
        let out = Recorder::default();
        let held = SharedHeld::default();
        out.connected.set(true);
        held.physical_key(&out, 0x57, true, 0x57);
        out.take();
        let lease = Lease::default();
        let mut pending = std::collections::VecDeque::from([
            event(0x2c, true),
            event(0xa2, true),
            event(0x5b, true),
        ]);
        assert!(take_ready(&mut pending, None, BARRIER_MS - 1).is_empty());
        let ready = take_ready(&mut pending, None, BARRIER_MS);
        assert_eq!(ready, [event(0xa2, true), event(0x5b, true)]);
        for event in ready {
            held.physical_key(&out, event.key.vk, event.key.down, event.key.vk);
        }
        assert_eq!(
            out.take(),
            [Sent::Key(0xa2, true, 2), Sent::Key(0x5b, true, 10)]
        );
        assert!(held.physical_down(0x57));
        assert!(lease.active(BARRIER_MS));
        assert!(pending.is_empty());
    }

    #[test]
    fn queued_control_alt_and_shift_keep_hook_order_across_frame_boundaries() {
        use std::collections::VecDeque;
        for modifier in 0xa0..=0xa5 {
            let first = Event {
                altgr: true,
                ..event(modifier, true)
            };
            let mut pending = VecDeque::from([first]);
            let before = take_ready(&mut pending, None, 0);
            pending.push_back(event(0x5b, true));
            let after = take_ready(&mut pending, None, 0);
            assert_eq!(
                [before, after]
                    .concat()
                    .iter()
                    .map(|e| e.key.vk)
                    .collect::<Vec<_>>(),
                [modifier, 0x5b]
            );
        }
        let control = Event {
            altgr: true,
            ..event(0xa2, true)
        };
        let mut pending = VecDeque::from([control]);
        assert!(take_ready(&mut pending, None, 0).is_empty());
        assert_eq!(take_ready(&mut pending, Some(control.key), 0), [control]);
        pending.extend([control, event(0xa5, true), event(0x5b, true)]);
        assert_eq!(
            take_ready(&mut pending, None, 0)
                .iter()
                .map(|e| e.key.vk)
                .collect::<Vec<_>>(),
            [0xa5, 0x5b]
        );
        assert!(pending.is_empty());
        assert!(Capture::new(true).altgr);
    }

    #[test]
    fn ordinary_winit_keys_are_barriers_between_hook_modifiers() {
        let mut pending = std::collections::VecDeque::from([
            event(0xa0, true),
            event(0x41, true),
            event(0xa0, false),
            event(0x5b, true),
        ]);
        assert_eq!(take_ready(&mut pending, None, 0), [event(0xa0, true)]);
        assert!(take_ready(&mut pending, None, 0).is_empty());
        assert!(take_ready(&mut pending, Some(event(0x41, true).key), 0).is_empty());
        assert_eq!(
            take_ready(&mut pending, None, 0),
            [event(0xa0, false), event(0x5b, true)]
        );
        assert!(pending.is_empty());
        let mut capture = Capture::new(false);
        let mut queued = Vec::new();
        assert!(!capture.consume(event(0x41, true), |e| {
            queued.push(e);
            true
        }));
        assert_eq!(queued, [event(0x41, true)]);
    }

    #[test]
    fn expiry_overflow_and_disconnection_fail_open() {
        let lease = Lease::default();
        assert!(lease.active(LEASE_MS - 1));
        assert!(!lease.active(LEASE_MS));
        lease.renew();
        lease.fail();
        assert!(!lease.active(0));
        let mut capture = Capture::new(false);
        assert!(!capture.consume(event(0x5b, false), |_| panic!("unowned up")));
        assert!(!capture.consume(event(0x5b, true), |_| false));
        assert!(!capture.consume(event(0x5b, false), |_| panic!("failed down")));
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_SIZE);
        for _ in 0..QUEUE_SIZE {
            tx.try_send(event(0xa0, true)).unwrap();
        }
        assert!(!capture.consume(event(0x5b, true), |e| tx.try_send(e).is_ok()));
        drop(rx);
        assert!(!capture.consume(event(0x5b, true), |e| tx.try_send(e).is_ok()));
    }

    #[test]
    fn every_modifier_precedes_win_and_only_owned_ups_are_consumed() {
        for modifier in 0xa0..=0xa5 {
            let mut capture = Capture::new(false);
            let mut sent = Vec::new();
            assert!(!capture.consume(event(modifier, true), |e| {
                sent.push(e);
                true
            }));
            assert!(capture.consume(event(0x5b, true), |e| {
                sent.push(e);
                true
            }));
            assert_eq!(
                sent.iter().map(|e| e.key.vk).collect::<Vec<_>>(),
                [modifier, 0x5b]
            );
            assert!(capture.consume(event(0x5b, false), |_| true));
            assert!(!capture.consume(event(0x5b, false), |_| panic!("duplicate up")));
        }
        for vk in [0x09, 0x20, 0x73] {
            let mut capture = Capture::new(false);
            assert!(capture.consume(event(vk, true), |_| true));
            assert!(capture.consume(
                Event {
                    alt: false,
                    ..event(vk, false)
                },
                |_| true
            ));
        }
    }

    #[test]
    fn altgr_filters_only_the_paired_synthetic_control_on_altgr_layouts() {
        for down in [false, true] {
            let control = event(0xa2, down);
            let alt = event(0xa5, down);
            assert!(Capture::fake_control(true, control, alt));
            assert!(!Capture::fake_control(false, control, alt));
            assert!(!Capture::fake_control(
                true,
                control,
                Event { time: 2, ..alt }
            ));
            assert!(!Capture::fake_control(true, control, event(0x5b, down)));
        }
    }
}
