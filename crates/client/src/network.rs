//! Small diagnosis surface shared by connection and in-session UI.
use latch_stream::network::NetworkStatus;
use latch_ui::{self as ui, Tone};

pub fn show(ui: &mut egui::Ui, status: &NetworkStatus) {
    for message in &status.messages {
        ui::notice(ui, Tone::Warning, message);
    }
    if let Some(url) = &status.portal_url {
        if ui.button("Open network sign-in").clicked() {
            latch_stream::network::recheck_portals();
            ui.ctx().open_url(egui::OpenUrl::new_tab(url.as_str()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{kittest::Queryable, Harness};

    #[test]
    fn healthy_network_has_no_warning_or_sign_in_action() {
        let harness = Harness::new_ui(|ui| show(ui, &NetworkStatus::default()));
        assert!(harness.query_by_label("Open network sign-in").is_none());
    }

    #[test]
    fn diagnosis_wraps_in_small_viewer_window() {
        let message = "This network inspects encrypted traffic. Latch refuses the substituted certificate (issuer Campus Certificate Authority).";
        let status = NetworkStatus {
            messages: vec![message.into()],
            ..Default::default()
        };
        let harness = Harness::builder()
            .with_size(egui::vec2(320.0, 240.0))
            .build_ui(|ui| show(ui, &status));
        let label = harness.get_by_label(message);
        let bounds = label.raw_bounds().unwrap();
        assert!(bounds.x1 <= 320.0);
        assert!(bounds.y1 <= 240.0);
    }
}
