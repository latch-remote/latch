use crate::{denied, invalid};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const MAGIC: &[u8; 8] = b"LATCHQ01";
const LEASE: u64 = 64 * 1024;

/// A locked, fixed-size durable reservation ledger. Unspent leases are charged.
pub(crate) struct Quota {
    file: File,
    month: u64,
    reserved: u64,
    budget: u64,
    failed: bool,
}
impl Quota {
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
    fn reserve_at(&mut self, month: u64) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("quota persistence failed, relay stopped"));
        }
        // A backwards clock never reopens a previously spent month.
        if month > self.month {
            self.month = month;
            self.reserved = 0;
        }
        if LEASE > self.budget.saturating_sub(self.reserved) {
            return Err(denied());
        }
        self.reserved += LEASE;
        self.persist()
    }
    pub fn credit(&mut self, credit: &mut u64, epoch: &mut u64) -> io::Result<()> {
        self.check()?;
        let now = month_now()?;
        // Leases from earlier months expire, including still-open connections.
        if *epoch != self.month.max(now) {
            *credit = 0;
        }
        if *credit < 2 * 16 * 1024 {
            self.reserve_at(now)?;
            *credit += LEASE;
            *epoch = self.month;
        }
        Ok(())
    }
    pub fn check(&self) -> io::Result<()> {
        if self.failed {
            Err(io::Error::other("quota persistence failed, relay stopped"))
        } else {
            Ok(())
        }
    }
    pub fn reserved(&self) -> u64 {
        self.reserved
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
            let mut q = Quota::open(&path, LEASE * 2, true).unwrap();
            month = q.month;
            q.reserve_at(month).unwrap();
            q.reserve_at(month).unwrap();
            assert!(q.reserve_at(month).is_err());
            assert!(Quota::open(&path, LEASE * 2, false).is_err());
        }
        {
            let mut q = Quota::open(&path, LEASE * 2, false).unwrap();
            assert!(q.reserve_at(month).is_err());
            assert!(q.reserve_at(month - 1).is_err());
            q.reserve_at(month + 1).unwrap();
            q.reserve_at(month).unwrap();
            assert!(q.reserve_at(month + 1).is_err());
        }
        assert!(Quota::open(&path, LEASE * 2, true).is_err());
        std::fs::write(&path, b"torn").unwrap();
        assert!(Quota::open(&path, LEASE * 2, false).is_err());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn failed_persistence_is_sticky() {
        let path = path();
        let mut q = Quota::open(&path, LEASE * 2, true).unwrap();
        q.file = File::open(&path).unwrap();
        assert!(q.reserve_at(q.month).is_err());
        assert!(q.failed);
        assert!(q.reserve_at(q.month + 1).is_err());
        drop(q);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn month_rollover_invalidates_live_socket_credit() {
        let path = path();
        let mut quota = Quota::open(&path, LEASE * 2, true).unwrap();
        let now = quota.month;
        quota.month = now - 1;
        quota.reserved = LEASE;
        quota.persist().unwrap();
        let mut credit = LEASE - 1;
        let mut epoch = now - 1;
        quota.credit(&mut credit, &mut epoch).unwrap();
        assert_eq!(epoch, now);
        assert_eq!(credit, LEASE);
        assert_eq!(quota.reserved, LEASE);
        drop(quota);
        std::fs::remove_file(path).unwrap();
    }
}
