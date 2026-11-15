//! Conditional byte token bucket; clean paths send immediately.
use std::time::{Duration, Instant};

pub(crate) struct Pacer {
    tokens: f64,
    updated: Instant,
    kbps: u32,
}
// Repay late scheduler wakes during a frame, with at most one 30-fps slot
// of credit. Idle capture time still receives the original one-ms allowance.
const MAX_CREDIT: Duration = Duration::from_millis(32);
const BURST: f64 = (2 * (latch_proto::MAX_DATAGRAM + 80)) as f64;
impl Pacer {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            tokens: BURST,
            updated: now,
            kbps: 0,
        }
    }
    pub(crate) fn begin_frame(&mut self, now: Instant, kbps: u32) {
        let rate = f64::from(kbps) * 1000.0 / 8.0;
        let burst = BURST.max(rate * 0.001 + (latch_proto::MAX_DATAGRAM + 80) as f64);
        self.tokens = (self.tokens
            + now.saturating_duration_since(self.updated).as_secs_f64() * rate)
            .min(burst);
        self.updated = now;
        self.kbps = kbps;
    }

    /// Spend elapsed-time credit, including late wakes within an active frame.
    /// Rate changes clamp credit immediately to the ordinary one-ms burst.
    /// The caller reloads the rate on every attempt.
    pub(crate) fn batch(
        &mut self,
        now: Instant,
        packets: &[Vec<u8>],
        kbps: u32,
    ) -> (usize, Duration) {
        if kbps == 0 {
            self.tokens = BURST;
            self.updated = now;
            self.kbps = 0;
            return (packets.len(), Duration::ZERO);
        }
        let rate = f64::from(kbps) * 1000.0 / 8.0;
        let credit = if self.kbps == kbps {
            MAX_CREDIT.as_secs_f64()
        } else {
            0.001
        };
        self.kbps = kbps;
        let burst = BURST.max(rate * credit + (latch_proto::MAX_DATAGRAM + 80) as f64);
        self.tokens = (self.tokens
            + now.saturating_duration_since(self.updated).as_secs_f64() * rate)
            .min(burst);
        self.updated = now;
        let mut bytes = 0.0;
        let mut count = 0;
        for packet in packets {
            let next = bytes + (packet.len() + 80) as f64;
            if next > self.tokens {
                break;
            }
            bytes = next;
            count += 1;
        }
        if count > 0 {
            self.tokens -= bytes;
            return (count, Duration::ZERO);
        }
        let total = packets.iter().map(|p| p.len() + 80).sum::<usize>() as f64;
        let target = total.min((rate * 0.001).max((packets[0].len() + 80) as f64));
        (
            0,
            Duration::from_secs_f64(((target - self.tokens) / rate).max(0.0))
                .min(Duration::from_millis(1)),
        )
    }
    #[cfg(test)]
    pub(crate) fn delay(&mut self, now: Instant, bytes: usize, kbps: u32) -> Duration {
        if kbps == 0 {
            self.tokens = BURST;
            self.updated = now;
            return Duration::ZERO;
        }
        let rate = f64::from(kbps.max(1)) * 1000.0 / 8.0;
        self.tokens = (self.tokens
            + now.saturating_duration_since(self.updated).as_secs_f64() * rate)
            .min(BURST.max(rate * 0.001 + (latch_proto::MAX_DATAGRAM + 80) as f64));
        self.updated = now;
        let bytes = (bytes + 80) as f64; // IP, UDP and tunnel allowance
        if self.tokens >= bytes {
            self.tokens -= bytes;
            Duration::ZERO
        } else {
            Duration::from_secs_f64((bytes - self.tokens) / rate)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clean_path_never_waits_even_for_large_keyframes() {
        let now = Instant::now();
        let mut p = Pacer::new(now);
        for _ in 0..10_000 {
            assert!(p.delay(now, 1200, 0).is_zero());
        }
    }
    #[test]
    fn keyframe_is_a_train_with_a_bounded_initial_burst() {
        let start = Instant::now();
        let mut now = start;
        let mut p = Pacer::new(now);
        for index in 0..500 {
            let delay = p.delay(now, 1200, 20_000);
            if index < 2 {
                assert!(delay.is_zero());
            }
            now += delay;
            if !delay.is_zero() {
                assert!(p
                    .delay(now + Duration::from_nanos(1), 1200, 20_000)
                    .is_zero());
            }
        }
        assert!(now - start > Duration::from_millis(90));
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    #[test]
    fn batches_bound_burst_and_apply_rate_cuts_immediately() {
        let at = Instant::now();
        let packets = vec![vec![0; 1200]; 32];
        let mut p = Pacer::new(at);
        assert_eq!(p.batch(at, &packets, 100_000).0, 2);
        let (count, wait) = p.batch(at, &packets, 100_000);
        assert_eq!(count, 0);
        assert!(wait <= Duration::from_millis(1));
        let later = at + Duration::from_millis(10);
        // A cut must clamp saved tokens to the *new* burst (two packets).
        assert_eq!(p.batch(later, &packets, 5_000).0, 2);
        assert_eq!(p.batch(later, &packets, 5_000).0, 0);
        assert_eq!(p.batch(later, &packets, 0).0, 32);
    }
    #[test]
    fn coarse_wakes_preserve_the_active_frame_rate() {
        let packets = vec![vec![0; 1200]; 32];
        for tick_ms in [1, 16, 30] {
            for kbps in [2000, 20_000, 60_000] {
                let start = Instant::now();
                let mut now = start;
                let mut pacer = Pacer::new(start);
                let mut bytes = 0;
                while now < start + Duration::from_secs(3) {
                    let (count, wait) = pacer.batch(now, &packets, kbps);
                    bytes += count * 1280;
                    if count == 0 {
                        now += wait.max(Duration::from_millis(tick_ms));
                    }
                }
                let achieved = bytes as f64 * 8.0 / 3.0 / 1000.0;
                eprintln!("{tick_ms} ms wakes at {kbps} kbps delivered {achieved:.0} kbps");
                assert!(
                    achieved >= f64::from(kbps) * 0.95,
                    "{tick_ms} ms wake at {kbps} kbps delivered {achieved:.0} kbps"
                );
                assert!(achieved <= f64::from(kbps) * 1.02);
            }
        }
    }

    #[test]
    fn capture_idle_and_long_stalls_keep_credit_bounded() {
        let start = Instant::now();
        let packets = vec![vec![0; 1200]; 1024];
        let mut pacer = Pacer::new(start);
        pacer.batch(start, &packets, 60_000);
        let later = start + Duration::from_secs(10);
        pacer.begin_frame(later, 60_000);
        assert_eq!(pacer.batch(later, &packets, 60_000).0, 6);
        let count = pacer
            .batch(later + Duration::from_secs(10), &packets, 60_000)
            .0;
        assert!(count * 1280 <= 60_000 * 32 / 8 + 1280);
        assert_eq!(
            pacer
                .batch(later + Duration::from_secs(10), &packets, 1000)
                .0,
            0
        );
    }

    #[test]
    fn high_rate_batches_reuse_elapsed_credit() {
        let at = Instant::now();
        let packets = vec![vec![0; 1200]; 32];
        let mut p = Pacer::new(at);
        p.batch(at, &packets, 100_000);
        assert_eq!(
            p.batch(at + Duration::from_millis(1), &packets, 100_000).0,
            9
        );
    }
}
