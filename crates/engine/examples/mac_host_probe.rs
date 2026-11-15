//! Local, bounded HID/cursor check. No service, input injection or permission
//! request. `cargo run -p latch-engine --example mac_host_probe -j 4`.

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use latch_engine::{
        mac,
        video::{Codec, VideoConfig},
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayBounds(id: u32) -> objc2_core_foundation::CGRect;
    }
    // SAFETY: these only query the current display geometry.
    let bounds = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    let display = latch_engine::input::DisplayRect {
        x: bounds.origin.x as i32,
        y: bounds.origin.y as i32,
        width: bounds.size.width as u32,
        height: bounds.size.height as u32,
    };
    let cursor = mac::system_cursor(display, 1920, 1080)
        .ok_or_else(|| anyhow::anyhow!("system cursor image unavailable"))?;
    let shape = cursor.shape.unwrap();
    println!(
        "AppKit cursor visible={}, {}x{}, hotspot {},{}, {} straight RGBA bytes",
        cursor.visible,
        shape.width,
        shape.height,
        shape.hot_x,
        shape.hot_y,
        shape.decode_rgba().map_err(anyhow::Error::msg)?.len()
    );
    mac::with_main_queue(move || {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        dispatch2::DispatchQueue::main().exec_async(move || {
            let started = Instant::now();
            let mut samples = 0;
            for _ in 0..100 {
                if mac::system_cursor(display, 1920, 1080).is_some() {
                    samples += 1;
                }
            }
            let _ = tx.send((samples, started.elapsed()));
        });
        let (samples, elapsed) = rx.recv_timeout(Duration::from_secs(5))?;
        anyhow::ensure!(samples == 100, "main-queue cursor sampling failed");
        println!(
            "background host main-queue sampling: {samples} shapes, mean {:.3} ms",
            elapsed.as_secs_f64() * 1000.0 / 100.0
        );
        match mac::gamepad::open(Arc::new(|_, _| {})) {
            Ok(mut pads) => {
                println!("virtual HID available, {} slots", pads.slots);
                pads.sink.update(
                    0,
                    Some(latch_proto::gamepad::PadKind::Xbox),
                    &Default::default(),
                )?;
                pads.sink.unplug_all();
            }
            Err(error) => println!("{error}"),
        }
        let mut video = match mac::open_video(&VideoConfig {
            color: Default::default(),
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 10000,
            codec: Codec::H264High,
        }) {
            Ok(video) => video,
            Err(error) => {
                println!("ScreenCaptureKit unavailable: {error:#}");
                return Ok(());
            }
        };
        println!(
            "ScreenCaptureKit opened, display {:?}",
            video.info().display
        );
        let supported = video.set_cursor_drawn(false);
        println!("separate cursor negotiation: {supported}");
        anyhow::ensure!(supported, "system cursor could not be read");
        let mut shapes = 0;
        let mut cursor_time = Duration::ZERO;
        for frame in 0..30 {
            let started = Instant::now();
            video.set_cursor_drawn(false);
            cursor_time += started.elapsed();
            let encoded = video.next(frame == 0, Duration::from_millis(20))?;
            anyhow::ensure!(!encoded.data.is_empty(), "empty captured picture");
            if let Some(update) = video.take_cursor() {
                if let Some(shape) = update.shape {
                    println!(
                        "cursor visible={}, {}x{}, hotspot {},{}, {} RGBA bytes",
                        update.visible,
                        shape.width,
                        shape.height,
                        shape.hot_x,
                        shape.hot_y,
                        shape.decode_rgba().map_err(anyhow::Error::msg)?.len()
                    );
                    shapes += 1;
                }
            }
        }
        anyhow::ensure!(shapes > 0, "no cursor shape produced");
        println!(
            "cursor sampling mean {:.3} ms across 30 frames",
            cursor_time.as_secs_f64() * 1000.0 / 30.0
        );
        anyhow::ensure!(
            video.set_cursor_drawn(true),
            "failed to restore composition"
        );
        println!(
            "30 real SCK/VideoToolbox frames, separate cursor and composition toggled successfully"
        );
        Ok(())
    })
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac_host_probe requires macOS");
}
