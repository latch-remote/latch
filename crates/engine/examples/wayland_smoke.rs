//! Exercise a private headless sway desktop with scripts/test-wayland.sh.
#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use anyhow::{ensure, Context};
    use latch_engine::linux::{Backend, WaylandCapture};
    use latch_engine::software::{Bgra, CpuCapture};
    use latch_engine::video::{Codec, VideoConfig};
    use std::process::Command;
    use std::time::{Duration, Instant};

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    ensure!(
        std::env::var("WLR_BACKENDS").as_deref() == Ok("headless"),
        "run scripts/test-wayland.sh on a private headless compositor"
    );
    ensure!(latch_engine::linux::backend() == Backend::Wayland);
    let mut capture = WaylandCapture::new(30)?;
    ensure!(capture.size() == (640, 360));
    let mut picture = Bgra::default();
    ensure!(capture.grab(&mut picture, Duration::from_secs(2))?);
    ensure!(capture.damage().is_none(), "first picture must be complete");
    let initial = picture.data.clone();
    ensure!(Command::new("swaymsg")
        .args(["output", "HEADLESS-1", "bg", "#986543", "solid_color"])
        .status()?
        .success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        capture.grab(&mut picture, Duration::from_millis(100))?;
        let pixel = &picture.data[0..3];
        if pixel == [0x43, 0x65, 0x98] {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "changed background never reached capture, pixel {pixel:?}"
        );
    }
    ensure!(
        picture.data != initial,
        "desktop changes must reach the consumer"
    );
    let config = VideoConfig {
        width: 640,
        height: 360,
        fps: 30,
        bitrate_kbps: 2000,
        codec: Codec::H264Baseline,
        color: Default::default(),
    };
    // The shipping entry point shares the existing portal session.
    let mut pipeline = latch_engine::linux::open_video(&config)?;
    let encoded = pipeline.next(true, Duration::from_secs(2))?;
    ensure!(encoded.keyframe && !encoded.data.is_empty());
    let mut decoder = latch_codec::h264::Decoder::new();
    let decoded = decoder
        .decode(&encoded.data)?
        .context("no decoded picture")?;
    ensure!(decoded.width == 640 && decoded.height == 360);
    println!("Wayland portal capture, changed pixels, and H.264 round trip passed");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {}
