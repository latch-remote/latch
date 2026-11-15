use crate::{denied, invalid};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

const MAGIC: &[u8; 8] = b"LATCHQ01";
pub(crate) const MIN_CREDIT: u64 = 2 * 16 * 1024;
const LEASE: u64 = 64 * 1024;
// Refill at half a batch. The pool and its pending reservation total at most 6 MiB.
const BATCH: u64 = 4 * 1024 * 1024;

/// Nonblocking leases from a bounded pool of durably reserved bytes.
pub(crate) struct Quota {
    requests: Option<SyncSender<u64>>,
    replies: Receiver<io::Result<Reservation>>,
    worker: Option<JoinHandle<()>>,
    month: u64,
    reserved: u64,
    available: u64,
    pending: bool,
    exhausted: bool,
    failed: bool,
}
#[derive(Debug)]
enum Reservation {
    Credit {
        month: u64,
        reserved: u64,
        bytes: u64,
    },
    Exhausted {
        month: u64,
    },
}
impl Quota {
    pub fn open(path: &Path, budget: u64, initialize: bool) -> io::Result<Self> {
        let mut ledger = Ledger::open(path, budget, initialize)?;
        let month = ledger.month;
        let reserved = ledger.reserved;
        let (requests, incoming) = mpsc::sync_channel(1);
        let (replies, completed) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("relay-quota".into())
            .spawn(move || {
                while let Ok(month) = incoming.recv() {
                    let result = ledger.reserve(month);
                    if replies.send(result).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests: Some(requests),
            replies: completed,
            worker: Some(worker),
            month,
            reserved,
            available: 0,
            pending: false,
            exhausted: false,
            failed: false,
        })
    }
    pub fn credit(&mut self, credit: &mut u64, epoch: &mut u64) -> io::Result<()> {
        self.check()?;
        let now = month_now()?;
        if now > self.month {
            self.month = now;
            self.available = 0;
            self.exhausted = false;
        }
        if *epoch != self.month {
            *credit = 0;
        }
        if *credit < MIN_CREDIT && self.available >= LEASE {
            self.available -= LEASE;
            *credit += LEASE;
            *epoch = self.month;
        }
        if self.available <= BATCH / 2 && !self.pending && !self.exhausted {
            self.requests
                .as_ref()
                .unwrap()
                .try_send(self.month)
                .map_err(|_| io::Error::other("quota worker stopped"))?;
            self.pending = true;
        }
        if *credit < MIN_CREDIT {
            return Err(if self.exhausted {
                denied()
            } else {
                io::ErrorKind::WouldBlock.into()
            });
        }
        Ok(())
    }
    pub fn check(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("quota persistence failed, relay stopped"));
        }
        match self.replies.try_recv() {
            Ok(result) => {
                self.pending = false;
                match result {
                    Ok(Reservation::Credit {
                        month,
                        reserved,
                        bytes,
                    }) => {
                        // A completion from before a month rollover remains
                        // charged to that ledger epoch but cannot fund new I/O.
                        if month == self.month {
                            self.available += bytes;
                        }
                        self.reserved = reserved;
                    }
                    Ok(Reservation::Exhausted { month }) => {
                        if month == self.month {
                            self.exhausted = true;
                        }
                    }
                    Err(error) => {
                        self.failed = true;
                        return Err(error);
                    }
                }
            }
            Err(TryRecvError::Empty) => (),
            Err(TryRecvError::Disconnected) => {
                self.failed = true;
                return Err(io::Error::other("quota worker stopped"));
            }
        }
        Ok(())
    }
    pub fn reservation_pending(&self) -> bool {
        self.pending
    }
    pub fn reserved(&self) -> u64 {
        self.reserved
    }
}
impl Drop for Quota {
    fn drop(&mut self) {
        drop(self.requests.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// A locked, fixed-size durable reservation ledger. Unspent leases are charged.
struct Ledger {
    file: File,
    month: u64,
    reserved: u64,
    budget: u64,
    failed: bool,
}
impl Ledger {
    pub fn open(path: &Path, budget: u64, initialize: bool) -> io::Result<Self> {
        if budget < LEASE {
            return Err(invalid(
                "monthly budget must cover at least one 64 KiB lease",
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        if initialize {
            options.create_new(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.try_lock().map_err(io::Error::other)?;
        if initialize {
            let mut result = Self {
                file,
                month: month_now()?,
                reserved: 0,
                budget,
                failed: false,
            };
            result.persist()?;
            #[cfg(unix)]
            File::open(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?
            .sync_all()?;
            return Ok(result);
        }
        if file.metadata()?.len() != 32 {
            return Err(invalid(
                "quota ledger length invalid, restore persistent ledger",
            ));
        }
        let mut b = [0; 32];
        file.read_exact(&mut b)?;
        let month = u64::from_be_bytes(b[8..16].try_into().unwrap());
        let reserved = u64::from_be_bytes(b[16..24].try_into().unwrap());
        let check = u64::from_be_bytes(b[24..32].try_into().unwrap());
        if &b[..8] != MAGIC
            || check != checksum(month, reserved)
            || !(23_000..100_000).contains(&month)
        {
            return Err(invalid("quota ledger damaged, restore persistent ledger"));
        }
        Ok(Self {
            file,
            month,
            reserved,
            budget,
            failed: false,
        })
    }
    fn persist(&mut self) -> io::Result<()> {
        let mut b = Vec::with_capacity(32);
        b.extend(MAGIC);
        b.extend(self.month.to_be_bytes());
        b.extend(self.reserved.to_be_bytes());
        b.extend(checksum(self.month, self.reserved).to_be_bytes());
        let result = (|| {
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(&b)?;
            self.file.sync_all()
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn reserve_at(&mut self, month: u64, bytes: u64) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("quota persistence failed, relay stopped"));
        }
        // A backwards clock never reopens a previously spent month.
        if month > self.month {
            self.month = month;
            self.reserved = 0;
        }
        if bytes > self.budget.saturating_sub(self.reserved) {
            return Err(denied());
        }
        self.reserved += bytes;
        self.persist()
    }
    fn reserve(&mut self, month: u64) -> io::Result<Reservation> {
        if self.failed {
            return Err(io::Error::other("quota persistence failed, relay stopped"));
        }
        let reserved = if month > self.month { 0 } else { self.reserved };
        let bytes = self.budget.saturating_sub(reserved).min(BATCH) / LEASE * LEASE;
        if bytes == 0 {
            return Ok(Reservation::Exhausted { month: self.month });
        }
        self.reserve_at(month, bytes)?;
        Ok(Reservation::Credit {
            month: self.month,
            reserved: self.reserved,
            bytes,
        })
    }
}
fn checksum(month: u64, reserved: u64) -> u64 {
    month.rotate_left(17) ^ reserved.rotate_right(9) ^ 0x714e_fc28_6ba3_d905
}
fn month_now() -> io::Result<u64> {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("system clock before epoch"))?
        .as_secs()
        / 86400;
    // Civil-from-days, proleptic Gregorian calendar, UTC. No timezone dependency.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    if month <= 2 {
        year += 1;
    }
    Ok(year * 12 + month - 1)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "latch-quota-{}-{}",
            std::process::id(),
            crate::crypto::random::<8>()
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ))
    }
    #[test]
    fn restart_and_backwards_month_do_not_reset_budget() {
        let path = path();
        let month;
        {
            let mut q = Ledger::open(&path, LEASE * 2, true).unwrap();
            month = q.month;
            q.reserve_at(month, LEASE).unwrap();
            q.reserve_at(month, LEASE).unwrap();
            assert!(q.reserve_at(month, LEASE).is_err());
            assert!(Ledger::open(&path, LEASE * 2, false).is_err());
        }
        {
            let mut q = Ledger::open(&path, LEASE * 2, false).unwrap();
            assert!(q.reserve_at(month, LEASE).is_err());
            assert!(q.reserve_at(month - 1, LEASE).is_err());
            q.reserve_at(month + 1, LEASE).unwrap();
            q.reserve_at(month, LEASE).unwrap();
            assert!(q.reserve_at(month + 1, LEASE).is_err());
        }
        assert!(Ledger::open(&path, LEASE * 2, true).is_err());
        std::fs::write(&path, b"torn").unwrap();
        assert!(Ledger::open(&path, LEASE * 2, false).is_err());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn failed_persistence_is_sticky() {
        let path = path();
        let mut q = Ledger::open(&path, LEASE * 2, true).unwrap();
        q.file = File::open(&path).unwrap();
        assert!(q.reserve_at(q.month, LEASE).is_err());
        assert!(q.failed);
        assert!(q.reserve_at(q.month + 1, LEASE).is_err());
        drop(q);
        std::fs::remove_file(path).unwrap();
    }
    fn controlled_quota(
        month: u64,
        available: u64,
    ) -> (Quota, Receiver<u64>, SyncSender<io::Result<Reservation>>) {
        let (requests, incoming) = mpsc::sync_channel(1);
        let (replies, completed) = mpsc::sync_channel(1);
        (
            Quota {
                requests: Some(requests),
                replies: completed,
                worker: None,
                month,
                reserved: available,
                available,
                pending: false,
                exhausted: false,
                failed: false,
            },
            incoming,
            replies,
        )
    }
    #[test]
    fn pending_reservations_do_not_block_or_lend_uncommitted_bytes() {
        let month = month_now().unwrap();
        let (mut quota, requests, replies) = controlled_quota(month, 0);
        let mut credit = 0;
        let mut epoch = 0;
        assert_eq!(
            quota.credit(&mut credit, &mut epoch).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        requests.try_recv().unwrap();
        assert_eq!(credit, 0);
        assert_eq!(quota.reserved(), 0);
        let mut existing = LEASE;
        let mut existing_epoch = month;
        quota.credit(&mut existing, &mut existing_epoch).unwrap();
        assert_eq!(existing, LEASE);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        replies
            .send(Ok(Reservation::Credit {
                month,
                reserved: BATCH,
                bytes: BATCH,
            }))
            .unwrap();
        quota.credit(&mut credit, &mut epoch).unwrap();
        assert_eq!(credit, LEASE);
        assert_eq!(epoch, month);
        assert_eq!(quota.reserved(), BATCH);
        assert_eq!(quota.available, BATCH - LEASE);
    }
    #[test]
    fn exhaustion_preserves_existing_credit_and_persistence_failure_is_sticky() {
        let month = month_now().unwrap();
        let (mut quota, requests, replies) = controlled_quota(month, LEASE);
        let mut credit = 0;
        let mut epoch = 0;
        quota.credit(&mut credit, &mut epoch).unwrap();
        requests.try_recv().unwrap();
        replies.send(Ok(Reservation::Exhausted { month })).unwrap();
        quota.credit(&mut credit, &mut epoch).unwrap();
        assert_eq!(credit, LEASE);
        let mut empty_credit = 0;
        let mut empty_epoch = 0;
        assert_eq!(
            quota
                .credit(&mut empty_credit, &mut empty_epoch)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );

        let (mut quota, requests, replies) = controlled_quota(month, 0);
        assert!(quota.credit(&mut empty_credit, &mut empty_epoch).is_err());
        requests.try_recv().unwrap();
        replies
            .send(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "disk failed",
            )))
            .unwrap();
        assert!(quota.check().is_err());
        assert!(quota.credit(&mut empty_credit, &mut empty_epoch).is_err());
        assert!(quota.failed);
    }
    #[test]
    fn month_rollover_invalidates_live_socket_and_pooled_credit() {
        let now = month_now().unwrap();
        let (mut quota, requests, replies) = controlled_quota(now - 1, LEASE);
        quota.exhausted = true;
        let mut credit = LEASE - 1;
        let mut epoch = now - 1;
        assert_eq!(
            quota.credit(&mut credit, &mut epoch).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(credit, 0);
        assert_eq!(quota.available, 0);
        requests.try_recv().unwrap();
        replies
            .send(Ok(Reservation::Credit {
                month: now,
                reserved: LEASE,
                bytes: LEASE,
            }))
            .unwrap();
        quota.credit(&mut credit, &mut epoch).unwrap();
        assert_eq!(epoch, now);
        assert_eq!(credit, LEASE);
        assert_eq!(quota.reserved(), LEASE);
    }
    #[test]
    fn late_completions_cannot_reopen_an_old_month_or_exhaust_a_new_one() {
        let month = month_now().unwrap();
        for completion in [
            Reservation::Credit {
                month,
                reserved: LEASE,
                bytes: LEASE,
            },
            Reservation::Exhausted { month },
        ] {
            let (mut quota, requests, replies) = controlled_quota(month, 0);
            let mut credit = 0;
            let mut epoch = 0;
            assert!(quota.credit(&mut credit, &mut epoch).is_err());
            assert_eq!(requests.try_recv().unwrap(), month);
            // The network loop observed the next month while persistence was
            // pending, followed by a backwards clock adjustment.
            quota.month = month + 1;
            replies.send(Ok(completion)).unwrap();
            assert_eq!(
                quota.credit(&mut credit, &mut epoch).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(credit, 0);
            assert_eq!(quota.available, 0);
            assert!(!quota.exhausted);
            assert_eq!(requests.try_recv().unwrap(), month + 1);
        }
    }
    #[test]
    fn reservations_batch_without_crossing_the_budget() {
        let path = path();
        let budget = BATCH + LEASE + 17;
        let mut ledger = Ledger::open(&path, budget, true).unwrap();
        assert!(matches!(
            ledger.reserve(ledger.month).unwrap(),
            Reservation::Credit { bytes: BATCH, .. }
        ));
        assert!(matches!(
            ledger.reserve(ledger.month).unwrap(),
            Reservation::Credit { bytes: LEASE, .. }
        ));
        assert!(matches!(
            ledger.reserve(ledger.month).unwrap(),
            Reservation::Exhausted { .. }
        ));
        drop(ledger);
        assert_eq!(
            Ledger::open(&path, budget, false).unwrap().reserved,
            BATCH + LEASE
        );
        std::fs::remove_file(path).unwrap();
    }
}
