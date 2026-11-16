//! All engine fixtures start with relay access disabled.
use super::Options;

pub fn options() -> Options {
    Options {
        relay: None,
        prepare: None,
        log: Box::new(|line| tracing::info!("{line}")),
        mic: None,
    }
}

#[test]
fn fixture_options_require_an_explicit_relay() {
    assert!(options().relay.is_none());
}
