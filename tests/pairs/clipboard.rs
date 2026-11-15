use super::*;
use crate::pair_format as format;
thread_local! {
    static CAPTURE: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn capture(text: &str) -> bool {
    CAPTURE.with_borrow_mut(|capture| {
        if let Some(values) = capture {
            values.push(text.to_string());
            true
        } else {
            false
        }
    })
}
#[test]
#[ignore = "clipboard artifact consumer, run through scripts/pairs.py"]
fn pair_clipboard() {
    let root = std::path::PathBuf::from(
        std::env::var_os("LATCH_PAIR_ARTIFACTS").expect("set LATCH_PAIR_ARTIFACTS"),
    );
    let mut consumed = 0;
    let mut failures = 0;
    for file in format::files(&root, ".input.json") {
        let recording: format::InputRecording = format::read(&file);
        if recording.target != std::env::consts::OS {
            continue;
        }
        consumed += 1;
        let row = serde_json::json!({"producer": recording.producer, "consumer": format::platform(), "case": "clipboard-line-endings"});
        if !format::cell("clipboard.jsonl", row, || {
            CAPTURE.set(Some(Vec::new()));
            write(&recording.clipboard).unwrap();
            let observed = CAPTURE.take().unwrap();
            let expected = if cfg!(windows) {
                "first\r\néλ你😀\r\n"
            } else {
                "first\néλ你😀\n"
            };
            let gap = format::CLIPBOARD_CRLF_GAP
                && recording.producer.starts_with("windows-")
                && !cfg!(windows);
            if gap {
                assert_eq!(observed, ["first\r\néλ你😀\r\n"]);
                assert_ne!(
                    observed,
                    [expected],
                    "known newline gap unexpectedly passes"
                );
            } else {
                assert_eq!(observed, [expected]);
            }
            format::result(
                "clipboard.jsonl",
                serde_json::json!({ "producer": recording.producer, "consumer": format::platform(), "case": "clipboard-line-endings", "status": if gap { "known-gap" } else { "passed" }, "observed": observed, "expected": [expected] }),
            );
        }) {
            failures += 1;
        }
    }
    assert!(consumed > 0);
    assert_eq!(failures, 0, "required clipboard cells failed");
}
