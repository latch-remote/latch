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
    // Change orientation while streaming. The source buffer keeps its raw
    // landscape axes, and SPA_META_VideoTransform carries the output rotation.
    let wallpaper =
        std::path::PathBuf::from(std::env::var("XDG_RUNTIME_DIR")?).join("quadrants.ppm");
    let mut image = b"P6\n32 32\n255\n".to_vec();
    for y in 0..32 {
        for x in 0..32 {
            let colors = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
            image.extend_from_slice(&colors[usize::from(x >= 16) + 2 * usize::from(y >= 16)]);
        }
    }
    std::fs::write(&wallpaper, image)?;
    ensure!(Command::new("swaymsg")
        .args(["output", "HEADLESS-1", "transform", "90"])
        .status()?
        .success());
    ensure!(Command::new("swaymsg")
        .args([
            "output",
            "HEADLESS-1",
            "bg",
            wallpaper.to_str().context("wallpaper path")?,
            "stretch"
        ])
        .status()?
        .success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        capture.grab(&mut picture, Duration::from_millis(100))?;
        if (picture.width, picture.height) == (360, 640) {
            let corners: Vec<_> = [(10, 10), (350, 10), (10, 630), (350, 630)]
                .into_iter()
                .map(|(x, y)| {
                    let at = y * picture.stride + x * 4;
                    picture.data[at..at + 3].to_vec()
                })
                .collect();
            let expected = [[0u8, 0, 255], [0, 255, 0], [255, 0, 0], [0, 255, 255]];
            if corners.iter().zip(expected).all(|(pixel, expected)| {
                pixel.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 8)
            }) {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "rotated output corners differ from the logical desktop: {corners:?}"
            );
        }
        ensure!(
            Instant::now() < deadline,
            "rotated output never reached portrait dimensions"
        );
    }
    ensure!(
        (capture.display().width, capture.display().height) == (360, 640),
        "pointer axes must follow the oriented picture"
    );
    std::fs::remove_file(wallpaper)?;
    // End the actual producer. Both Session.Closed and a paused PipeWire
    // consumer must terminate capture, even on an otherwise idle desktop.
    let portal_pid = std::env::var("LATCH_TEST_PORTAL_PID")?;
    ensure!(Command::new("kill").arg(portal_pid).status()?.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match capture.grab(&mut picture, Duration::from_millis(100)) {
            Err(error) => {
                ensure!(
                    error.is::<latch_engine::video::CaptureEnded>(),
                    "unexpected shutdown error: {error:#}"
                );
                break;
            }
            Ok(_) => ensure!(
                Instant::now() < deadline,
                "producer shutdown left capture alive"
            ),
        }
    }
    println!(
        "Wayland idle startup, changed pixels, H.264, rotated output, and producer shutdown passed"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {}
