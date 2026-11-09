use std::time::{Duration,Instant};
fn main(){for us in [10,100,500,1000]{let mut v=Vec::new();for _ in 0..1000{let at=Instant::now();std::thread::sleep(Duration::from_micros(us));v.push(at.elapsed().as_nanos() as f64/1000.0);}v.sort_by(f64::total_cmp);println!("sleep_us={us} actual_p50/p95/max={:.1}/{:.1}/{:.1}",v[500],v[950],v[999]);}}
