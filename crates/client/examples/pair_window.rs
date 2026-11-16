//! Disposable deterministic capture source for the pair CI workflow.
use std::{path::PathBuf, time::Duration};

struct Window {
    ready: PathBuf,
}
impl eframe::App for Window {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                ui.painter()
                    .rect_filled(rect, 0., egui::Color32::from_rgb(32, 64, 128));
                let stripe = egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(rect.width(), rect.height() / 4.),
                );
                ui.painter()
                    .rect_filled(stripe, 0., egui::Color32::from_rgb(180, 100, 40));
            });
        std::fs::write(&self.ready, "ready").unwrap();
        ctx.request_repaint_after(Duration::from_millis(40));
    }
}

fn main() -> eframe::Result {
    let ready = std::env::args_os()
        .nth(1)
        .expect("supply a readiness-file path");
    let mut options = latch_client::native_options();
    options.viewport = options
        .viewport
        .with_fullscreen(true)
        .with_decorations(false);
    eframe::run_native(
        "Latch pair capture fixture",
        options,
        Box::new(move |_| {
            Ok(Box::new(Window {
                ready: ready.into(),
            }))
        }),
    )
}
