"""Regression checks for the verdicts that gate CI."""
import json
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch

from middlebox import alpns
from netlab import classify, evaluate, main, run_scenario


def healthy_events():
    events = [{"kind": "connected", "elapsed_ms": 20},
              {"kind": "first_frame", "elapsed_ms": 50}]
    for second in range(19):
        events.append({"kind": "sample", "elapsed_ms": second * 1000,
                       "decoded_seq": second * 30, "path": "udp", "audio_click_ms": [20.]})
        events.append({"kind": "input", "elapsed_ms": second * 1000, "round_trip_ms": 40})
    return events


class VerdictTests(unittest.TestCase):
    def test_real_decode_and_input_required(self):
        events = healthy_events()
        result = evaluate("a", events, 18, 0)
        self.assertEqual(result["failures"], [])
        for event in events:
            if event["kind"] == "sample":
                event["decoded_seq"] = 0
        result = evaluate("a", events, 18, 0)
        self.assertIn("tail_decoded_fps_at_least_15", result["failures"])
        self.assertIn("no_decoded_stall_over_5s", result["failures"])

    def test_exit_zero_after_termination_fails(self):
        events = healthy_events() + [{"kind": "terminated", "elapsed_ms": 5000, "message": "encoder failed"}]
        self.assertIn("no_session_termination", evaluate("a", events, 18, 0)["failures"])

    def test_baseline_relay_dial_fails(self):
        self.assertIn("zero_relay_connections", evaluate("a", healthy_events(), 18, 1)["failures"])

    def test_expected_failures_are_exact_and_unexpected_pass_is_red(self):
        expected = {"failures": ["first_frame_deadline"]}
        self.assertEqual(classify({"failures": ["first_frame_deadline"]}, expected), "EXPECTED-FAILURE")
        self.assertEqual(classify({"failures": []}, expected), "UNEXPECTED-PASS")
        self.assertEqual(classify({"failures": ["other"]}, expected), "FAIL")
        self.assertEqual(classify({"infrastructure_error": "missing nft"}, expected), "ERROR")

    def test_expected_failure_requires_observed_error(self):
        expected = {"failures": ["first_frame_deadline"], "error_pattern": "no picture arrived"}
        result = {"failures": ["first_frame_deadline"], "errors": [{"message": "encoder broke"}]}
        self.assertEqual(classify(result, expected), "FAIL")
        result["errors"] = [{"message": "no picture arrived"}]
        self.assertEqual(classify(result, expected), "EXPECTED-FAILURE")

    def test_setup_error_and_unexpected_pass_fail_the_job(self):
        expected = {"i": {"failures": ["first_frame_deadline"]}}
        with tempfile.TemporaryDirectory() as directory, patch("netlab.Lab") as lab:
            lab.return_value.execute.side_effect = RuntimeError("private PulseAudio sink unavailable")
            result = run_scenario("i", Path(directory), expected)
            self.assertEqual(result["status"], "ERROR")
            lab.return_value.close.assert_called_once()
            for status in ("ERROR", "UNEXPECTED-PASS"):
                with self.subTest(status=status), patch("netlab.run_scenario", return_value={
                        "scenario": "i", "name": "proxy", "status": status}), \
                        patch("netlab.os.geteuid", return_value=0), \
                        patch("netlab.sys.platform", "linux"), \
                        patch("netlab.command", return_value="test revision"), \
                        patch("netlab.sys.argv", ["netlab", "--scenarios", "i", "--output", directory]):
                    self.assertEqual(main(), 1)
                    saved = json.loads((Path(directory) / "results.json").read_text())
                    self.assertEqual(saved["results"][0]["status"], status)

    def test_security_requires_user_facing_error_and_no_frames(self):
        events = healthy_events()
        result = evaluate("k", events, 18, 1)
        self.assertIn("no_untrusted_media", result["failures"])
        self.assertIn("clear_security_error_under_8s", result["failures"])

    def test_client_hello_alpn_parser(self):
        protocols = b"\x02h2\x08http/1.1"
        extension = struct.pack("!H", len(protocols)) + protocols
        extensions = struct.pack("!HH", 16, len(extension)) + extension
        body = b"\x03\x03" + bytes(32) + b"\x00\x00\x02\x13\x01\x01\x00" + struct.pack("!H", len(extensions)) + extensions
        handshake = b"\x01" + len(body).to_bytes(3, "big") + body
        record = b"\x16\x03\x01" + len(handshake).to_bytes(2, "big") + handshake
        self.assertEqual(alpns(record), ["h2", "http/1.1"])


if __name__ == "__main__":
    unittest.main()
