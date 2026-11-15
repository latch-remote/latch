//! TCP forwarding with a shared uplink serializer and silent loss of existing
//! connections. TLS and the relay daemon remain unmodified.
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct Uplink {
    kbps: u32,
    next: Instant,
}
impl Uplink {
    pub fn new(kbps: u32) -> Self {
        Self {
            kbps,
            next: Instant::now(),
        }
    }
    pub fn schedule(&mut self, bytes: usize, max_queue: Duration) -> Option<Instant> {
        let now = Instant::now();
        let end = self.next.max(now)
            + Duration::from_secs_f64((bytes + 80) as f64 * 8.0 / (f64::from(self.kbps) * 1000.0));
        if end.saturating_duration_since(now) > max_queue {
            return None;
        }
        self.next = end;
        Some(end)
    }
}

pub struct Gateway {
    pub port: u16,
    pub uplink: Option<Arc<parking_lot::Mutex<Uplink>>>,
    accepted: Arc<AtomicU64>,
    blackhole_through: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Gateway {
    pub fn start(target: u16, kbps: Option<u32>) -> Self {
        Self::with_delay(target, kbps, Duration::ZERO)
    }
    pub fn with_delay(target: u16, kbps: Option<u32>, delay: Duration) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let uplink = kbps.map(|rate| Arc::new(parking_lot::Mutex::new(Uplink::new(rate))));
        let accepted = Arc::new(AtomicU64::new(0));
        let blackhole_through = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (accepted, blackhole, stop, uplink) = (
                accepted.clone(),
                blackhole_through.clone(),
                stop.clone(),
                uplink.clone(),
            );
            thread::spawn(move || {
                let mut workers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((client, _)) => {
                            let daemon = TcpStream::connect((Ipv4Addr::LOCALHOST, target)).unwrap();
                            client.set_nodelay(true).unwrap();
                            daemon.set_nodelay(true).unwrap();
                            let id = accepted.fetch_add(1, Ordering::AcqRel) + 1;
                            for (mut source, mut sink, limit) in [
                                (
                                    client.try_clone().unwrap(),
                                    daemon.try_clone().unwrap(),
                                    uplink.clone(),
                                ),
                                (daemon, client, None),
                            ] {
                                let (stop, blackhole) = (stop.clone(), blackhole.clone());
                                workers.push(thread::spawn(move || {
                                    source
                                        .set_read_timeout(Some(Duration::from_millis(1)))
                                        .unwrap();
                                    sink.set_write_timeout(Some(Duration::from_millis(100)))
                                        .unwrap();
                                    let mut bytes = [0; 16 * 1024];
                                    let mut pending = std::collections::VecDeque::new();
                                    let mut queued_bytes = 0;
                                    let mut eof = false;
                                    while !stop.load(Ordering::Acquire) {
                                        while pending
                                            .front()
                                            .is_some_and(|(due, _)| *due <= Instant::now())
                                        {
                                            let (_, packet): (Instant, Vec<u8>) =
                                                pending.pop_front().unwrap();
                                            queued_bytes -= packet.len();
                                            if id > blackhole.load(Ordering::Acquire)
                                                && sink.write_all(&packet).is_err()
                                            {
                                                return;
                                            }
                                        }
                                        // Bound buffering while allowing propagation delay to
                                        // overlap reads. Sleeping per read would cap throughput
                                        // at one buffer per delay interval.
                                        if eof && pending.is_empty() {
                                            break;
                                        }
                                        if eof || queued_bytes >= 1024 * 1024 {
                                            thread::sleep(Duration::from_millis(1));
                                            continue;
                                        }
                                        match source.read(&mut bytes) {
                                            Ok(0) => eof = true,
                                            Ok(count) => {
                                                if id <= blackhole.load(Ordering::Acquire) {
                                                    continue;
                                                }
                                                let due = limit.as_ref().map_or_else(
                                                    Instant::now,
                                                    |limit| {
                                                        limit
                                                            .lock()
                                                            .schedule(count, Duration::MAX)
                                                            .unwrap()
                                                    },
                                                ) + delay;
                                                pending.push_back((due, bytes[..count].to_vec()));
                                                queued_bytes += count;
                                            }
                                            Err(error)
                                                if matches!(
                                                    error.kind(),
                                                    io::ErrorKind::WouldBlock
                                                        | io::ErrorKind::TimedOut
                                                ) => {}
                                            Err(_) => break,
                                        }
                                    }
                                    let _ = source.shutdown(Shutdown::Both);
                                    let _ = sink.shutdown(Shutdown::Both);
                                }));
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1))
                        }
                        Err(error) => panic!("gateway accept {error}"),
                    }
                }
                for worker in workers {
                    worker.join().unwrap();
                }
            })
        };
        Self {
            port,
            uplink,
            accepted,
            blackhole_through,
            stop,
            worker: Some(worker),
        }
    }
    pub fn blackhole_existing_connections(&self) {
        self.blackhole_through
            .store(self.accepted.load(Ordering::Acquire), Ordering::Release);
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}
