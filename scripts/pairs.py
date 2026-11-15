#!/usr/bin/env python3
"""Build once, exchange versioned session artifacts, exercise every pair."""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
PLATFORMS = ("windows-x86_64", "macos-aarch64", "linux-x86_64", "linux-aarch64")
SYSTEM = {"Darwin": "macos", "Windows": "windows", "Linux": "linux"}[platform.system()]
ARCH = {"arm64": "aarch64", "aarch64": "aarch64", "amd64": "x86_64", "x86_64": "x86_64"}[platform.machine().lower()]
IDENTITY = f"{SYSTEM}-{ARCH}"
EXE = ".exe" if SYSTEM == "windows" else ""
REQUIRED = {
    "linux": {"software", "capture"},
    "macos": {"software", "native", "capture"},
    "windows": {"software", "native"},
}
# Only these absent hosted devices/backends may produce unavailable rows.
UNAVAILABLE = {
    "linux": {"native": "Linux hosted runners have no native encoder device"},
    "macos": {},
    "windows": {"capture": "0x887A0004"},
}
INPUT_CASES = ("ansi-a", "azerty-a-at-q", "qwertz-z-at-y", "shift-a", "control-a", "alt-a", "right-shift-identity", "held-super-z", "unicode-commit", "relative-pointer", "wheel-line", "wheel-two-half-notches", "absolute-pointer-button", "button-left", "button-right", "button-middle", "button-x1", "button-x2", "gamepad-report", "clipboard-line-endings")
SCHEDULES = ("clean", "loss-reorder", "late-audio")


def executable(binaries, name):
    path = binaries / (name + EXE)
    if not path.is_file():
        raise RuntimeError(f"required pair executable is missing: {path}")
    try:
        path.chmod(path.stat().st_mode | 0o111)
    except OSError as error:
        raise RuntimeError(f"cannot restore execute permission for required pair executable {path}: {error}") from error
    if not os.access(path, os.X_OK):
        raise RuntimeError(f"required pair executable is not executable: {path}")
    return path


def run(args, *, env=None, timeout=180):
    print("+", " ".join(map(str, args)), flush=True)
    subprocess.run(list(map(str, args)), cwd=ROOT, env=env, check=True, timeout=timeout)


def build(binaries):
    binaries.mkdir(parents=True, exist_ok=True)
    # Codecs remain optimized by the workspace profile. Dependency optimization
    # and DWARF dominate cold hosted builds and provide no benefit to these cases.
    common = ["--locked", "-j", "4", "--config", "profile.dev.package.'*'.opt-level=0", "--config", "profile.dev.debug=0"]
    commands = [
        ["test", "--no-run", "--lib", "-p", "latch-engine", "-p", "latch-stream", "-p", "latch-client", "-p", "latch-host"],
        ["test", "--no-run", "-p", "latch-engine", "--test", "loopback"],
        ["build", "-p", "latch-engine", "--example", "serve"],
        ["build", "-p", "latch-client", "--example", "pair_window"],
    ]
    for command in commands:
        proc = subprocess.Popen(["cargo", *command, *common, "--message-format=json"], cwd=ROOT, stdout=subprocess.PIPE, text=True)
        for line in proc.stdout:
            message = json.loads(line)
            if message.get("reason") == "compiler-message":
                print(message["message"].get("rendered", ""), end="", flush=True)
            executable = message.get("executable")
            if executable:
                name = message["target"]["name"].removeprefix("latch_")
                destination = binaries / (name + EXE)
                staging = destination.with_name(destination.name + ".new")
                shutil.copy2(executable, staging)
                # A fresh inode also avoids macOS retaining the previous
                # executable's code-signature cache across local rebuilds.
                staging.replace(destination)
        if proc.wait():
            raise RuntimeError("pair test binary build failed")


def execute(binaries, name, test, env):
    path = executable(binaries, name)
    listing = subprocess.check_output([str(path), test, "--list"], text=True, env=env)
    assert "1 test," in listing, f"expected one named pair test, got {listing}"
    run([path, test, "--ignored", "--nocapture", "--test-threads=1"], env=env)


@contextlib.contextmanager
def xvfb(env):
    if SYSTEM != "linux":
        yield
        return
    socket = Path("/tmp/.X11-unix/X9")
    if socket.exists():
        raise RuntimeError("DISPLAY :9 is already in use, pair tests require their own Xvfb")
    proc = subprocess.Popen(["Xvfb", ":9", "-screen", "0", "640x360x24", "-nolisten", "tcp", "-ac"])
    try:
        deadline = time.monotonic() + 10
        while not socket.exists():
            if proc.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("isolated Xvfb did not start")
            time.sleep(0.05)
        env.update(DISPLAY=":9", LATCH_PAIR_XVFB="1")
        yield
    finally:
        proc.terminate()
        proc.wait(timeout=10)


@contextlib.contextmanager
def capture_window(binaries, output, env, enabled):
    if SYSTEM == "linux" or not enabled:
        yield
        return
    ready = output / "window.ready"
    ready.unlink(missing_ok=True)
    path = executable(binaries, "pair_window")
    with (output / "window.log").open("w") as log:
        proc = subprocess.Popen([str(path), str(ready)], stdout=log, stderr=log, env=env)
        try:
            deadline = time.monotonic() + 10
            while not ready.exists() and proc.poll() is None and time.monotonic() < deadline:
                time.sleep(0.05)
            if ready.exists() and proc.poll() is None:
                env["LATCH_PAIR_CAPTURE_WINDOW"] = "1"
                time.sleep(0.5)
            yield
        finally:
            if proc.poll() is None:
                proc.terminate()
            proc.wait(timeout=10)
            ready.unlink(missing_ok=True)


def rows(root):
    return [json.loads(line) for path in root.rglob("*.jsonl") for line in path.read_text(encoding="utf-8").splitlines() if line]


def record(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    for pattern in ("*.session.json", "*.input.json", "gamepad.pad.json", "manifest.json", "results/*.jsonl"):
        for path in output.glob(pattern):
            path.unlink()
    env = dict(os.environ, LATCH_PAIR_RECORD=str(output), LATCH_PAIR_RESULTS=str(output / "results"))
    env["LATCH_PAIR_X11_HELPER"] = str(executable(args.binaries, "serve"))
    with xvfb(env), capture_window(args.binaries, output, env, args.capture_window):
        execute(args.binaries, "engine", "pair_record", env)
        execute(args.binaries, "client", "pair_input_record", env)
        execute(args.binaries, "stream", "pair_gamepad_record", env)
        # Existing loopback suite verifies the synthetic key/picture and pad
        # round trips. It runs in addition to the two-process audio session.
        path = executable(args.binaries, "loopback")
        run([path, "--nocapture", "--test-threads=1"], env=env)
        serve = executable(args.binaries, "serve")
        env.update(LATCH_TEST_PATTERN="still", LATCH_TEST_AUDIO="clicks", LATCH_PAIR_LIVE_PORT="47863")
        with (output / "live.log").open("w") as log:
            host = subprocess.Popen([str(serve), "--port", "47863"], stdout=log, stderr=log, env=env)
            try:
                deadline = time.monotonic() + 10
                while True:
                    try:
                        with socket.create_connection(("127.0.0.1", 47863), timeout=0.2):
                            break
                    except OSError:
                        if host.poll() is not None or time.monotonic() > deadline:
                            raise RuntimeError("live host did not listen")
                        time.sleep(0.05)
                execute(args.binaries, "stream", "pair_live", env)
            finally:
                host.terminate()
                host.wait(timeout=10)
    files = {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in output.glob("*.json") if path.name != "manifest.json"}
    manifest = {"version": 1, "producer": IDENTITY, "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(), "files": files, "capabilities": rows(output / "results")}
    require_capabilities(manifest, args)
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")


def require_capabilities(manifest, args):
    system = manifest["producer"].split("-")[0]
    required = REQUIRED[system].copy()
    if system == "macos" and not (args.grid or args.capture_window or os.environ.get("GITHUB_ACTIONS") == "true"):
        required.remove("capture")
    for mode in required:
        assert any(row.get("mode") == mode and row["status"] == "recorded" for row in manifest["capabilities"]), f"{manifest['producer']} requires {mode} on this runner"
    if args.grid or os.environ.get("GITHUB_ACTIONS") == "true":
        for row in manifest["capabilities"]:
            if row["status"] == "unavailable":
                reason = UNAVAILABLE[system].get(row["mode"])
                assert reason and reason in row["reason"], f"unexpected unavailable capability on {manifest['producer']}: {row}"


def verify(args):
    manifests = list(args.artifacts.rglob("manifest.json"))
    identities = []
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    for path in manifests:
        manifest = json.loads(path.read_text(encoding="utf-8"))
        assert manifest["version"] == 1
        assert manifest["revision"] == revision, "recording was produced by another revision"
        identities.append(manifest["producer"])
        require_capabilities(manifest, args)
        assert "software.session.json" in manifest["files"]
        assert "gamepad.pad.json" in manifest["files"]
        for mode in ("software", "native", "capture"):
            capability = [row for row in manifest["capabilities"] if row.get("mode") == mode]
            assert len(capability) == 1, f"missing or duplicate {mode} capability result"
            assert capability[0]["status"] in ("recorded", "unavailable")
            assert (f"{mode}.session.json" in manifest["files"]) == (capability[0]["status"] == "recorded")
            if capability[0]["status"] == "unavailable":
                assert capability[0]["reason"], "unavailable capability needs an observed reason"
        for name, digest in manifest["files"].items():
            assert Path(name).name == name, "artifact path must be a filename"
            assert hashlib.sha256((path.parent / name).read_bytes()).hexdigest() == digest, f"artifact integrity failed, {path.parent / name}"
        for target in ("windows", "macos", "linux"):
            assert f"{target}.input.json" in manifest["files"]
        if manifest["producer"].startswith("linux-"):
            assert "capture.session.json" in manifest["files"], "Xvfb capture is required"
    assert identities, "no producer manifests"
    assert set(identities) <= set(PLATFORMS), f"unknown producers, {identities}"
    expected = PLATFORMS if args.grid else sorted(set(identities))
    assert sorted(identities) == sorted(expected), f"producer set {identities}, expected {expected}"
    results = args.output.resolve()
    results.mkdir(parents=True, exist_ok=True)
    for name in ("media.jsonl", "input.jsonl", "clipboard.jsonl"):
        (results / name).unlink(missing_ok=True)
    env = dict(os.environ, LATCH_PAIR_ARTIFACTS=str(args.artifacts.resolve()), LATCH_PAIR_RESULTS=str(results))
    env["LATCH_PAIR_X11_HELPER"] = str(executable(args.binaries, "serve"))
    failures = []
    with xvfb(env):
        for binary, test in [("stream", "pair_replay"), ("engine", "pair_native_input"), ("host", "pair_clipboard")]:
            try:
                execute(args.binaries, binary, test, env)
            except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
                failures.append(test)
    summary(args.artifacts, results, expected)
    if failures:
        raise RuntimeError(f"required consumers failed, {failures}")


def summary(artifacts, results, expected):
    observations = rows(results)
    lines = [f"## Pair interoperability on {IDENTITY}", "", "| Host recording | Mode | Viewer decoder | Clean | Loss/reorder | Late audio |", "|---|---|---|---|---|---|"]
    missing = []
    counts = {"passed": 0, "known-gap": 0, "failed": 0, "not-run": 0, "unavailable": 0}

    def status(matches, required):
        if not required:
            label = "unavailable"
        elif any(r["status"] == "failed" for r in matches):
            label = "failed"
        elif any(r["status"] == "passed" for r in matches):
            label = "passed"
        elif any(r["status"] == "known-gap" for r in matches):
            label = "known-gap"
        else:
            label = "not-run"
        counts[label] += 1
        return label

    native_unavailable = next((r for r in observations if r.get("consumer") == IDENTITY and r.get("decoder") == "native" and r["status"] == "unavailable"), None)
    if native_unavailable and SYSTEM != "linux":
        missing.append((IDENTITY, "required native decoder unavailable", native_unavailable["reason"]))
    inputs = []
    for producer in expected:
        manifest = next(json.loads(p.read_text(encoding="utf-8")) for p in artifacts.rglob("manifest.json") if json.loads(p.read_text(encoding="utf-8"))["producer"] == producer)
        for mode in ("software", "native", "capture"):
            available = f"{mode}.session.json" in manifest["files"]
            for decoder in ("software", "native"):
                required = available and (decoder == "software" or SYSTEM != "linux")
                labels = []
                for schedule in SCHEDULES:
                    matches = [r for r in observations if r.get("consumer") == IDENTITY and r.get("producer") == producer and r.get("mode") == mode and r.get("decoder") == decoder and r.get("schedule") == schedule]
                    label = status(matches, required)
                    labels.append(label)
                    if label in ("failed", "not-run"):
                        missing.append((producer, mode, decoder, schedule, label))
                lines.append(f"| {producer} | {mode} | {decoder} | {' | '.join(labels)} |")
        for case in INPUT_CASES:
            matches = [r for r in observations if r.get("consumer") == IDENTITY and r.get("producer") == producer and r.get("case") == case]
            label = status(matches, True)
            inputs.append(f"| {producer} | {IDENTITY} | {case} | {label} |")
            if label in ("failed", "not-run"):
                missing.append((producer, case, label))
    run_count = counts["passed"] + counts["known-gap"] + counts["failed"]
    lines.extend(["", f"Run {run_count}, passed {counts['passed']}, known-gap {counts['known-gap']}, failed {counts['failed']}, not-run {counts['not-run']}, unavailable {counts['unavailable']}.", "", "### Input, clipboard and gamepad results", "", "| Viewer | Host | Case | Result |", "|---|---|---|---|", *inputs])
    lines.extend(["", "### Capability limits and known gaps", ""])
    for path in artifacts.rglob("producer.jsonl"):
        for row in rows(path.parent):
            if row["status"] == "unavailable": lines.append(f"- {row['producer']} {row['mode']} — {row['reason']}")
    for limit in sorted({row["native_bus"] for row in observations if "native_bus" in row}):
        lines.append(f"- Gamepad injection — {limit}")
    for row in observations:
        if row["status"] in ("unavailable", "known-gap", "failed"):
            lines.extend(["```json", json.dumps(row, ensure_ascii=False), "```", ""])
    text = "\n".join(lines) + "\n"
    (results / "summary.md").write_text(text, encoding="utf-8")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as output:
            output.write(text)
    assert not missing, f"required grid cells missing or failing, {missing}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("build", "record", "verify", "local"))
    parser.add_argument("--binaries", type=Path, default=ROOT / "target/pairs/bin")
    parser.add_argument("--output", type=Path, default=ROOT / "target/pairs/recordings")
    parser.add_argument("--artifacts", type=Path, default=ROOT / "target/pairs/recordings")
    parser.add_argument("--runner", choices=PLATFORMS)
    parser.add_argument("--grid", action="store_true")
    parser.add_argument("--capture-window", action="store_true", default=os.environ.get("GITHUB_ACTIONS") == "true")
    args = parser.parse_args()
    args.binaries = args.binaries.resolve()
    assert args.runner is None or args.runner == IDENTITY, f"runner architecture {IDENTITY}, expected {args.runner}"
    os.chdir(ROOT)
    started = time.monotonic()
    if args.command in ("build", "local"): build(args.binaries)
    if args.command in ("record", "local"): record(args)
    if args.command == "local":
        args.artifacts = args.output
        args.output = args.output.with_name(args.output.name + "-results")
    if args.command in ("verify", "local"): verify(args)
    print(f"pair {args.command} completed in {time.monotonic() - started:.1f}s", flush=True)


if __name__ == "__main__":
    main()
