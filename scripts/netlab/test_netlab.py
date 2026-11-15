"""Regression checks for the verdicts that gate CI."""
import json
from pathlib import Path
import struct
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

from middlebox import alpns
from netlab import Lab, classify, encoder_tail, evaluate, main, run_scenario, summary
from stage_artifacts import stage


def healthy_events():
    events = [{"kind": "connected", "elapsed_ms": 20},
              {"kind": "first_frame", "elapsed_ms": 50}]
    for second in range(19):
        events.append({"kind": "sample", "elapsed_ms": second * 1000,
                       "decoded_seq": second * 30, "path": "udp", "audio_click_ms": [20.]})
        events.append({"kind": "input", "elapsed_ms": second * 1000, "round_trip_ms": 40})
    return events


class VerdictTests(unittest.TestCase):
    def test_encoder_diagnostics_use_only_the_measured_tail(self):
        log = "\n".join(
            f"2026-11-15T00:00:{second:02d}Z DEBUG latch_engine::software: "
            f"software encoder output qp={qp} bytes=23000 target_kbps=5650"
            for second, qp in [(1, 18), (11, 19), (12, 30), (17, 32), (19, 18)])
        result = encoder_tail(log, 1794700800, 18)
        self.assertEqual(result["tail_encoder_frames"], 2)
        self.assertEqual(result["tail_encoder_qp_min"], 30)
        self.assertEqual(result["tail_encoder_qp_mean"], 31)
        self.assertEqual(result["tail_encoder_target_mbps"], 5.65)
        self.assertEqual(encoder_tail("", 0, 18), {})
        table = summary([{**result, "scenario": "a", "name": "clean UDP", "status": "PASS"}])
        self.assertIn("QP min/mean", table)
        self.assertIn("30/31.0", table)

    def test_loss_qualification_requires_real_decode_and_disables_relay(self):
        with tempfile.TemporaryDirectory() as directory:
            lab = Lab("d1", Path(directory))
            process = Mock()
            process.wait.return_value = 0
            with patch.object(lab, "spawn", return_value=process) as spawn, \
                    patch.object(lab, "ns") as ns, patch("netlab.read_events", return_value=healthy_events()):
                lab.qualify_udp_loss()
                self.assertIn("--no-relay", spawn.call_args.args[2])
                process.wait.assert_called_once_with(timeout=52)
                self.assertEqual(ns.call_count, 4)
            with patch.object(lab, "spawn", return_value=process), \
                    patch("netlab.read_events", return_value=[]):
                with self.assertRaisesRegex(RuntimeError, "decoded no real media"):
                    lab.qualify_udp_loss()

    def test_cleanup_continues_after_diagnostic_timeouts(self):
        with tempfile.TemporaryDirectory() as directory:
            lab = Lab("a", Path(directory))
            lab.created = [lab.names["v"]]
            lab.processes = [object()]
            with patch.object(lab, "ns", side_effect=subprocess.TimeoutExpired("ss", 3)), \
                    patch.object(lab, "stop_process") as stop, patch.object(lab, "run") as run:
                with self.assertRaisesRegex(RuntimeError, "cleanup errors"):
                    lab.close()
                stop.assert_called_once_with(lab.processes[0])
                run.assert_any_call("ip", "netns", "del", lab.names["v"], check=False)
                self.assertEqual(run.call_count, 3)

    def test_middlebox_evidence_is_required_even_for_an_expected_failure(self):
        expected = {"failures": ["no_picture"], "required_assertions": ["viewer_reached_middlebox"]}
        result = {"failures": ["no_picture"], "assertions": {"viewer_reached_middlebox": False}}
        self.assertEqual(classify(result, expected), "FAIL")
        result["assertions"]["viewer_reached_middlebox"] = True
        self.assertEqual(classify(result, expected), "EXPECTED-FAILURE")

    def test_loss_scenarios_require_requested_video_quality_on_either_path(self):
        events = [{"kind": "connected", "elapsed_ms": 20}, {"kind": "first_frame", "elapsed_ms": 50}]
        for tick in range(161):
            events.append({"kind": "sample", "elapsed_ms": tick * 250, "decoded_seq": tick * 7.5,
                           "path": "udp", "mbps": 5.5, "audio_click_ms": [20.]})
            events.append({"kind": "input", "elapsed_ms": tick * 250, "round_trip_ms": 40})
        self.assertEqual(evaluate("d1", events, 40, 0)["failures"], [])
        for event in events:
            if event["kind"] == "sample":
                event["path"] = "relay"
                event["mbps"] = 1
        result = evaluate("d2", events, 40, 1)
        self.assertIn("tail_video_at_least_75pct_requested", result["failures"])

    def test_artifact_copies_are_readable_and_exclude_private_files(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source"
            output = Path(directory) / "artifact"
            scenario = source / "i"
            scenario.mkdir(parents=True)
            for name in ("proxy-access.log", "result.json", "events.jsonl", "quota", "relay-key.pem"):
                path = scenario / name
                path.write_text("test")
                path.chmod(0o600)
            stage(source, output)
            self.assertEqual(sorted(path.name for path in (output / "i").iterdir()),
                             ["events.jsonl", "proxy-access.log", "result.json"])
            for path in (output / "i").iterdir():
                self.assertEqual(path.stat().st_mode & 0o777, 0o644)
            for path in scenario.iterdir():
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)

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

    def test_observed_quality_signatures_keep_other_failures_red(self):
        expected = {"failures": ["video_bitrate"], "related_failures": ["fps"]}
        self.assertEqual(classify({"failures": ["fps", "video_bitrate"]}, expected), "EXPECTED-FAILURE")
        self.assertEqual(classify({"failures": ["fps"]}, expected), "FAIL")
        self.assertEqual(classify({"failures": ["connected", "video_bitrate"]}, expected), "FAIL")

    def test_setup_error_and_unexpected_pass_fail_the_job(self):
        expected = {"i": {"failures": ["first_frame_deadline"]}}
        with tempfile.TemporaryDirectory() as directory, patch("netlab.Lab") as lab, \
                patch.dict("netlab.os.environ", {"GITHUB_STEP_SUMMARY": ""}):
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
