//! Measurement-only JSONL observer. It uses the real session and decoder.
use latch_stream::{Event, FrameSlot, Session};
use std::io::Write;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

pub fn run(
    session: &Session,
    frames: &FrameSlot,
    events: &Receiver<Event>,
    start: Instant,
    seconds: u64,
    path: &str,
) -> anyhow::Result<()> {
    let mut output = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut emit = |value: serde_json::Value| -> anyhow::Result<()> {
        serde_json::to_writer(&mut output, &value)?;
        writeln!(output)?;
        output.flush()?;
        Ok(())
    };
    let mut first = false;
    let mut white = None;
    let mut pending = None;
    let mut next_key = start + Duration::from_secs(1);
    let mut next_sample = start;
    let mut path = "udp";
    let mut sequence = 0;
    let mut connected = false;
    while start.elapsed() < Duration::from_secs(seconds) {
        let elapsed_ms = start.elapsed().as_millis();
        for event in events.try_iter() {
            let kind = match &event {
                Event::Connected => "stream_ready",
                Event::Terminated { .. } => "terminated",
                Event::Failed { .. } => "failed",
                Event::Notice(_) => "notice",
                Event::NoAudio(_) => "audio_error",
                _ => "event",
            };
            emit(
                serde_json::json!({"kind":kind,"elapsed_ms":elapsed_ms,"message":format!("{event:?}")}),
            )?;
        }
        let stats = session.stats();
        if !connected && !stats.codec.is_empty() {
            connected = true;
            emit(serde_json::json!({"kind":"connected","elapsed_ms":elapsed_ms}))?;
        }
        let current = if session.relay_active() {
            "relay"
        } else {
            "udp"
        };
        if current != path {
            path = current;
            emit(serde_json::json!({"kind":"path","elapsed_ms":elapsed_ms,"path":path}))?;
        }
        if frames.seq() != sequence {
            sequence = frames.seq();
            emit(
                serde_json::json!({"kind":"decoded_frame","elapsed_ms":elapsed_ms,"decoded_seq":sequence}),
            )?;
            if !first {
                first = true;
                emit(serde_json::json!({"kind":"first_frame","elapsed_ms":elapsed_ms}))?;
            }
            if let Some(frame) = frames.take() {
                let current_white = super::square_is_white(&frame);
                frames.recycle(frame);
                if white.is_some_and(|old| old != current_white) {
                    if let Some(pressed) = pending.take() {
                        let pressed: Instant = pressed;
                        emit(
                            serde_json::json!({"kind":"input","elapsed_ms":elapsed_ms,"round_trip_ms":pressed.elapsed().as_secs_f64()*1000.0,"path":path}),
                        )?;
                        session.input().key(0x41, false, 0);
                    }
                }
                white = Some(current_white);
            }
        }
        if pending.is_some_and(|pressed: Instant| pressed.elapsed() > Duration::from_secs(2)) {
            pending = None;
            session.input().key(0x41, false, 0);
            emit(serde_json::json!({"kind":"input_timeout","elapsed_ms":elapsed_ms,"path":path}))?;
        }
        if first && connected && pending.is_none() && Instant::now() >= next_key {
            pending = Some(Instant::now());
            next_key = Instant::now() + Duration::from_secs(1);
            session.input().key(0x41, true, 0);
        }
        if Instant::now() >= next_sample {
            next_sample = Instant::now() + Duration::from_millis(250);
            emit(serde_json::json!({
                "kind":"sample", "elapsed_ms":elapsed_ms, "decoded_seq":sequence,
                "fps":stats.fps, "path":path, "mbps":stats.mbps,
                "carrier":session.relay_connection_info().as_ref().map(latch_stream::network::carrier_label),
                "requested_bitrate_kbps":stats.requested_bitrate_kbps,
                "applied_bitrate_kbps":stats.applied_bitrate_kbps,
                "loss_pct":stats.loss_pct, "audio":stats.audio,
                "audio_click_ms":latch_stream::audio::take_probe(),
                "finished":session.finished(),
            }))?;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    session.stop();
    let stopped = Instant::now();
    while !session.finished() && stopped.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::ensure!(
        session.finished(),
        "netlab session did not stop within five seconds"
    );
    Ok(())
}
