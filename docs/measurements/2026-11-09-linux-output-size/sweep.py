#!/usr/bin/env python3
"""Repeatable software worker sweep; timings are accepted only below load 3."""
import argparse
import csv
import hashlib
import io
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys
import tarfile
import time

SIZES = [(1920, 1080), (2560, 1440), (3840, 2160)]
WORKERS = [2, 4, 6, 8, 10]
ROOT = Path(__file__).resolve().parents[3]


def load():
    status = subprocess.check_output(["uptime"], text=True).strip()
    return float(re.search(r"load averages?:\s*([\d.]+)", status)[1]), status


def spread(values):
    return f"{statistics.median(values):.3f} [{min(values):.3f}, {max(values):.3f}]"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wait-minutes", type=float, default=40)
    parser.add_argument("--results", type=Path, default=Path("/tmp/latch-output-size-results"))
    parser.add_argument("--baseline", type=Path, help="Original pipeline_bench with LATCH_BENCH_THREADS override")
    parser.add_argument("--baseline-ref", help="Build an original benchmark from this local git revision")
    parser.add_argument("--deterministic", action="store_true", help="Disable wall-clock adaptation; measure bytes/PSNR under any load")
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--frames", type=int, default=30)
    args = parser.parse_args()
    os.chdir(ROOT)
    args.results.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["PATH"] = str(Path.home() / ".cargo/bin") + os.pathsep + env["PATH"]
    env["TMPDIR"] = "/tmp"
    if not args.no_build:
        subprocess.run(["cargo", "build", "--release", "-j", "2", "-p", "latch-engine", "--example", "pipeline_bench", "-p", "latch-codec", "--example", "encode_bench"], env=env, check=True)
    if args.baseline_ref and not args.deterministic:
        baseline = args.results.resolve() / "baseline"
        baseline.mkdir(exist_ok=True)
        archive = args.results / "baseline.tar"
        subprocess.run(["git", "archive", "--format=tar", "--output", str(archive), args.baseline_ref], check=True)
        with tarfile.open(archive) as source:
            source.extractall(baseline, filter="data")
        software = baseline / "crates/engine/src/software.rs"
        source = software.read_text()
        old = "std::thread::available_parallelism().map_or(1, |n| n.get().min(4))"
        assert source.count(old) == 2, "Baseline changed: inspect the two worker overrides"
        replacement = 'std::env::var("LATCH_BENCH_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| ' + old + ')'
        software.write_text(source.replace(old, replacement))
        subprocess.run(["cargo", "build", "--release", "-j", "2", "--manifest-path", str(baseline / "Cargo.toml"), "-p", "latch-engine", "--example", "pipeline_bench"], env=env, check=True)
        args.baseline = baseline / "target/release/examples/pipeline_bench"
    deadline = time.monotonic() + args.wait_minutes * 60

    def run(name, command, extra_env=None):
        path = args.results / (name + ".json")
        if path.exists():
            cached = json.loads(path.read_text())
            if cached["valid"] and cached["command"] == command:
                return cached
        while True:
            before_load, before = load()
            if not args.deterministic and before_load >= 3:
                if time.monotonic() >= deadline:
                    raise TimeoutError("No quiet window left: pending rows were not measured")
                print(f"Waiting for load <3; current {before_load}", file=sys.stderr, flush=True)
                time.sleep(min(20, max(0, deadline - time.monotonic())))
                continue
            trial_env = env | (extra_env or {})
            result = subprocess.run(command, text=True, capture_output=True, env=trial_env, check=True)
            after_load, after = load()
            valid = args.deterministic or after_load < 3
            record = dict(command=command, environment=extra_env or {}, before=before, after=after,
                          valid=valid, stdout=result.stdout, stderr=result.stderr)
            if args.deterministic:
                record["sha256"] = hashlib.sha256(Path(command[-1]).read_bytes()).hexdigest()
            path.write_text(json.dumps(record, indent=2) + "\n")
            if valid:
                print(name, file=sys.stderr, flush=True)
                return record
            if time.monotonic() >= deadline:
                raise TimeoutError("Load rose during the trial; row rejected")

    print("| Path | Picture | Content | Workers | fps median [min, max] | Encode ms median [min, max] | Convert ms median [min, max] |", flush=True) if not args.deterministic else print("| Picture | Content | Workers | kbps | PSNR dB | Average QP | Identical repeats |", flush=True)
    print("|---|---|---|---:|---:|---:|---:|", flush=True)
    for w, h in SIZES:
        for workers in WORKERS:
            for kind in ["motion", "scroll"]:
                rows, hashes = [], []
                for repeat in range(3):
                    name = f"{'deterministic' if args.deterministic else 'encode'}-{w}-{workers}-{kind}-{repeat}"
                    command = ["target/release/examples/encode_bench", "--width", str(w), "--height", str(h), "--fps", "60", "--kbps", "20000", "--frames", str(args.frames), "--threads", str(workers), "--kind", kind]
                    if args.deterministic:
                        command += ["--budget", "0", "--output", str(args.results / (name + ".h264"))]
                    record = run(name, command)
                    rows.append(list(csv.DictReader(io.StringIO(record["stdout"])))[-1])
                    hashes.append(record.get("sha256"))
                if args.deterministic:
                    assert len(set(hashes)) == 1, f"Nondeterministic output: {name}"
                    row = rows[0]
                    print(f"| {w}×{h} | {kind} | {workers} | {row['achieved_kbps']} | {row['psnr']} | {row['average_qp']} | yes |", flush=True)
                else:
                    ms = [float(r["steady_mean_ms"]) for r in rows]
                    print(f"| encode | {w}×{h} | {kind} | {workers} | {spread([1000 / v for v in ms])} | {spread(ms)} | — |", flush=True)
            if args.deterministic:
                continue
            for label, binary in [("pipeline", Path("target/release/examples/pipeline_bench"))] + ([("before", args.baseline)] if args.baseline and workers in [2, 4] else []):
                for kind in ["noise", "sweep"]:
                    fps, enc, convert = [], [], []
                    for repeat in range(3):
                        command = [str(binary), "--width", str(w), "--height", str(h), "--fps", "500", "--seconds", "1", "--motion", kind, "--software"]
                        if label != "before":
                            command += ["--threads", str(workers)]
                        record = run(f"{label}-{w}-{workers}-{kind}-{repeat}", command, {"LATCH_BENCH_THREADS": str(workers)})
                        text = record["stdout"]
                        fps.append(float(re.search(r": ([\d.]+) fps", text)[1]))
                        enc.append(float(re.search(r"encode\s+mean=([\d.]+)", text)[1]))
                        convert.append(float(re.search(r"convert\s+mean=([\d.]+)", text)[1]))
                    print(f"| {label} | {w}×{h} | {kind} | {workers} | {spread(fps)} | {spread(enc)} | {spread(convert)} |", flush=True)


if __name__ == "__main__":
    try:
        main()
    except TimeoutError as error:
        print(f"PENDING: {error}", file=sys.stderr)
        sys.exit(2)
