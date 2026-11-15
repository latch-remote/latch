#!/usr/bin/env python3
"""Bounded paired native/WebSocket/CONNECT comparison in isolated namespaces."""
import argparse
import json
import os
from pathlib import Path
import signal
import time

from netlab import Lab, ROOT, STOP, read_events


PATHS = {"150": (150, 0), "150-loss": (150, 0.5), "300": (300, 0), "20": (20, 0)}


def cpu_seconds(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def measure(lab, relay, args, path, rate, carrier):
    rtt, loss = PATHS[path]
    for role, device in (("v", "tail"), ("c", "viewer")):
        lab.ns(role, "tc", "qdisc", "del", "dev", device, "root", check=False)
        lab.ns(role, "tc", "qdisc", "add", "dev", device, "root", "netem",
               "limit", "20000", "delay", f"{rtt / 2}ms", "loss", f"{loss}%")
    name = f"{path}-{rate}-{carrier}"
    command = [args.probe, "relay.netlab.test", str(args.seconds), str(rate), "60", str(args.warmup),
               "native" if carrier == "native" else "websocket"]
    if carrier == "connect":
        command.append("10.77.2.1:3128")
    before = cpu_seconds(relay.pid)
    started = time.monotonic()
    client = lab.spawn("v", name, command, env={"PAIRED_PROBE_CA": str(lab.output / "relay-ca.pem")})
    client_cpu = 0
    while client.poll() is None:
        if STOP.is_set() or time.monotonic() - started > args.seconds + 25:
            lab.stop_process(client)
            raise RuntimeError("probe exceeded its bounded runtime or was interrupted")
        try:
            client_cpu = cpu_seconds(client.pid)
        except FileNotFoundError:
            pass
        time.sleep(0.1)
    wall = time.monotonic() - started
    events = read_events(lab.output / f"{name}.log") if client.returncode == 0 else []
    # Error lines are deliberately retained in the raw log.
    if not events:
        events = []
        for line in (lab.output / f"{name}.log").read_text().splitlines():
            if line.startswith("{"):
                events.append(json.loads(line))
    result = next((event for event in reversed(events) if event.get("final")), {})
    result.update(path=path, offered_mbps=rate, carrier=carrier, exit_code=client.returncode,
                  wall_s=wall, client_cpu_percent=100 * client_cpu / wall,
                  relay_cpu_percent=100 * (cpu_seconds(relay.pid) - before) / wall)
    for role, device in (("v", "tail"), ("c", "viewer")):
        stats = lab.ns(role, "tc", "-s", "-j", "qdisc", "show", "dev", device)
        (lab.output / f"{name}-{role}-qdisc.json").write_text(stats)
    print(json.dumps(result), flush=True)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seconds", type=int, default=20)
    parser.add_argument("--warmup", type=int, default=5)
    parser.add_argument("--paths", default=",".join(PATHS))
    parser.add_argument("--rates", default="5,20,50")
    parser.add_argument("--carriers", default="native,websocket,connect")
    parser.add_argument("--probe", type=Path, default=ROOT / "target/release/examples/paired_probe")
    parser.add_argument("--relay", type=Path, default=ROOT / "target/release/latch-relay")
    args = parser.parse_args()
    if not 1 <= args.seconds <= 60:
        parser.error("seconds must be in 1..60")
    if not 0 <= args.warmup < args.seconds:
        parser.error("warmup must be nonnegative and shorter than the run")
    if not set(args.paths.split(",")) <= PATHS.keys():
        parser.error("unknown path")
    if not set(args.carriers.split(",")) <= {"native", "websocket", "connect"}:
        parser.error("unknown carrier")
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda *_: STOP.set())
    lab = Lab("wss", args.output)
    try:
        lab.topology()
        lab.certificate("relay", "relay.netlab.test")
        # Keep rapid failed comparisons from exhausting admission rate limits.
        # All transport deadlines and byte/queue bounds retain their defaults.
        limits = args.output / "limits.json"
        limits.write_text(json.dumps({"accepts_per_ip_per_minute": 1000, "accepts_per_minute": 1000}))
        relay = lab.spawn("i", "relay", [args.relay, "--listen", "0.0.0.0:443",
            "--cert", args.output / "relay.pem", "--key", args.output / "relay-key.pem",
            "--quota-file", args.output / "quota", "--monthly-bytes", str(16 * 1024**3),
            "--initialize-quota", "--limits-json", limits])
        config = args.output / "tinyproxy.conf"
        config.write_text(f"Port 3128\nListen 10.77.2.1\nTimeout 30\nAllow 10.77.2.0/24\nConnectPort 443\nLogLevel Error\nLogFile \"{args.output}/proxy.log\"\nPidFile \"{args.output}/proxy.pid\"\n")
        lab.spawn("c", "proxy", ["tinyproxy", "-d", "-c", config])
        time.sleep(0.5)
        results = []
        for path in args.paths.split(","):
            for rate in map(int, args.rates.split(",")):
                for carrier in args.carriers.split(","):
                    result = measure(lab, relay, args, path, rate, carrier)
                    results.append(result)
                    (args.output / "summary.json").write_text(json.dumps(results, indent=2) + "\n")
    finally:
        lab.close()


if __name__ == "__main__":
    main()
