use std::{net::ToSocketAddrs, sync::{Arc, Barrier}, thread, time::Instant};
fn measure(workers: usize) {
    let barrier = Arc::new(Barrier::new(workers));
    let started = Instant::now();
    let handles: Vec<_> = (0..workers).map(|_| {
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            (0..20).map(|_| {
                let start = Instant::now();
                let result = ("localhost", 443).to_socket_addrs().map(|addresses| addresses.count());
                (start.elapsed().as_secs_f64()*1000.0, result.is_ok())
            }).collect::<Vec<_>>()
        })
    }).collect();
    let samples: Vec<_> = handles.into_iter().flat_map(|handle| handle.join().unwrap()).collect();
    let errors = samples.iter().filter(|sample| !sample.1).count();
    let mut times: Vec<_> = samples.into_iter().map(|sample| sample.0).collect();
    times.sort_by(f64::total_cmp);
    println!("workers={workers} samples={} errors={errors} wall_ms={:.3} mean_ms={:.3} p50_ms={:.3} p95_ms={:.3} max_ms={:.3}",
        times.len(), started.elapsed().as_secs_f64()*1000.0,
        times.iter().sum::<f64>()/times.len() as f64,
        times[times.len()/2],times[times.len()*95/100],times[times.len()-1]);
}
fn main() { measure(1); measure(16); }
