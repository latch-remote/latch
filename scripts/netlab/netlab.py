#!/usr/bin/env python3
"""Exercise real Latch binaries in disposable Linux network namespaces."""
import argparse
import concurrent.futures
from datetime import datetime
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import traceback

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
SCENARIOS = {
    "a": ("clean UDP", 18),
    "b": ("UDP denied at startup", 22),
    "c": ("UDP outage and restoration", 95),
    "d1": ("17% bursty UDP loss", 40),
    "d2": ("UDP policed to 3 Mbps", 35),
    "e": ("campus TCP 443 only", 22),
    "f": ("unreachable relay, healthy UDP", 18),
    "g": ("relay black hole during stream", 32),
    "h": ("UDP MTU black hole", 25),
    "i": ("explicit CONNECT proxy only", 22),
    "i2": ("CONNECT proxy requires unavailable credentials", 16),
    "j": ("HTTPS-only ALPN inspector", 22),
    "k": ("TLS interception", 16),
    "l": ("captive portal", 16),
    "l2": ("captive portal opens during connection", 30),
    "m": ("both endpoints NAT64, well-known /96", 25),
    "m2": ("both endpoints NAT64, global-looking /96", 25),
    "n": ("relay TCP MSS black hole", 26),
}


STOP = threading.Event()


def command(*args, input=None, check=True, timeout=15):
    return subprocess.run(list(map(str, args)), input=input, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                          timeout=timeout, check=check).stdout


def read_events(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def encoder_tail(log, started, duration):
    """Use host wall timestamps so qualification frames cannot enter the tail."""
    frames = []
    for line in re.sub(r"\x1b\[[0-9;]*m", "", log).splitlines():
        match = re.search(r"^(\S+) .*software encoder output qp=(\d+) bytes=(\d+) target_kbps=(\d+)", line)
        if match:
            timestamp, qp, size, target = match.groups()
            elapsed = datetime.fromisoformat(timestamp.replace("Z", "+00:00")).timestamp() - started
            if duration - 6 <= elapsed <= duration:
                frames.append((int(qp), int(size), int(target)))
    if not frames:
        return {}
    return {
        "tail_encoder_frames": len(frames),
        "tail_encoder_qp_min": min(frame[0] for frame in frames),
        "tail_encoder_qp_mean": sum(frame[0] for frame in frames) / len(frames),
        "tail_encoder_target_mbps": sum(frame[2] for frame in frames) / len(frames) / 1000,
        "tail_encoded_video_mbps": sum(frame[1] for frame in frames) * 8 / 6 / 1e6,
    }


def police_statistics(filters):
    stats = [action.get("stats", {}) for rule in filters for action in rule.get("options", {}).get("actions", [])
             if action.get("kind") == "police"]
    return {key: sum(stat.get(key, 0) for stat in stats) for key in ("bytes", "packets", "drops")}


def evaluate(scenario, events, duration, relay_connections):
    """Stable assertion IDs are also the exact expected-failure signature."""
    failures = []
    checks = {}

    def require(name, condition):
        checks[name] = bool(condition)
        if not condition:
            failures.append(name)

    connected = [e["elapsed_ms"] for e in events if e["kind"] == "connected"]
    first = [e["elapsed_ms"] for e in events if e["kind"] == "first_frame"]
    samples = [e for e in events if e["kind"] == "sample"]
    terminations = [e for e in events if e["kind"] in ("terminated", "failed")]
    inputs = [e for e in events if e["kind"] == "input"]
    paths = [{"elapsed_ms": 0, "path": "udp"}] + [
        {"elapsed_ms": e["elapsed_ms"], "path": e["path"]} for e in events if e["kind"] == "path"]
    metrics = {}
    fps = []
    for left, right in zip(samples, samples[1:]):
        delta = right["elapsed_ms"] - left["elapsed_ms"]
        if delta:
            fps.append({"elapsed_ms": right["elapsed_ms"], "fps":
                        (right["decoded_seq"] - left["decoded_seq"]) * 1000 / delta,
                        "path": right["path"]})
    require("connected_under_3s", bool(connected) and connected[0] <= 3000)
    if scenario not in ("k", "l", "i2"):
        require("no_session_termination", not terminations)
    require("sampler_completed", bool(samples) and samples[-1]["elapsed_ms"] >= (duration - 1) * 1000)
    if scenario in ("k", "l", "i2"):
        require("no_untrusted_media", not first)
        expression = r"certificate|unknown.?issuer|tls.{0,30}(trust|intercept)" if scenario == "k" else (r"proxy authentication required" if scenario == "i2" else r"captive|portal|sign.in")
        require("clear_security_error_under_8s", any(
            re.search(expression, e.get("message", ""), re.I) and e["elapsed_ms"] <= 8000 for e in events))
    else:
        first_limit = 3000 if scenario in ("a", "f") else 7000
        if scenario == "l2":
            first_limit = 14000
            require("portal_reported_before_open", any("sign-in" in e.get("message", "") and e["elapsed_ms"] < 8000 for e in events))
            require("stream_starts_after_portal_opens", bool(first) and first[0] >= 10000)
        if scenario in ("h", "n"):
            first_limit = 4000
        require("first_frame_deadline", bool(first) and first[0] <= first_limit)
        tail = [e for e in samples if e["elapsed_ms"] >= (duration - 6) * 1000]
        tail_fps = ((tail[-1]["decoded_seq"] - tail[0]["decoded_seq"]) * 1000 /
                    (tail[-1]["elapsed_ms"] - tail[0]["elapsed_ms"])) if len(tail) > 1 else 0
        require("tail_decoded_fps_at_least_15", tail_fps >= 15)
        progress = []
        previous = 0
        decoded = [e for e in events if e["kind"] == "decoded_frame"]
        for e in decoded or samples:
            if e["decoded_seq"] > previous:
                progress.append(e["elapsed_ms"])
                previous = e["decoded_seq"]
        gaps = ([b - a for a, b in zip(progress, progress[1:])] +
                ([duration * 1000 - progress[-1]] if progress else [duration * 1000]))
        maximum_gap = max(gaps)
        tail_start = (duration - 6) * 1000
        tail_progress = [at for at in progress if at >= tail_start]
        tail_bounds = [tail_start, *tail_progress, duration * 1000]
        tail_gaps = [b - a for a, b in zip(tail_bounds, tail_bounds[1:])]
        require("no_decoded_stall_over_5s", bool(progress) and maximum_gap <= 5000)
        late_inputs = [e for e in inputs if e["elapsed_ms"] >= (duration - 7) * 1000]
        require("tail_input_round_trips", len(late_inputs) >= 3 and max(e["round_trip_ms"] for e in late_inputs) <= 500)
        clicks = [value for e in samples for value in e.get("audio_click_ms", []) if e["elapsed_ms"] >= (duration - 7) * 1000]
        require("tail_audio_clicks", len(clicks) >= 8)
        metrics = {"tail_decoded_fps": tail_fps, "max_decoded_stall_ms": maximum_gap,
                   "tail_input_round_trips": len(late_inputs), "tail_audio_clicks": len(clicks),
                   "tail_input_max_ms": max((e["round_trip_ms"] for e in late_inputs), default=None),
                   "decoded_stall_count": sum(gap > 500 for gap in gaps),
                   "tail_decoded_stall_count": sum(gap > 500 for gap in tail_gaps),
                   "tail_max_decoded_stall_ms": max(tail_gaps),
                   "tail_video_mbps": sum(e.get("mbps", 0) for e in tail) / max(1, len(tail)),
                   "requested_video_mbps": 6,
                   "tail_path": tail[-1]["path"] if tail else None}
        if scenario in ("d1", "d2"):
            require("tail_decoded_fps_at_least_25", tail_fps >= 25)
            require("no_tail_decoded_stall_over_1s", bool(tail_progress) and max(tail_gaps) <= 1000)
            require("at_most_one_tail_decoded_stall", metrics["tail_decoded_stall_count"] <= 1)
            require("tail_video_at_least_75pct_requested", metrics["tail_video_mbps"] >= 4.5)
            require("tail_path_stable", bool(tail) and len({e["path"] for e in tail}) == 1)
        if scenario in ("a", "f"):
            require("UDP_only", all(p["path"] == "udp" for p in paths))
            require("zero_relay_connections", relay_connections == 0)
        if scenario in ("b", "e", "i", "l2", "j", "m", "m2", "n"):
            require("relay_active", bool(tail) and all(e["path"] == "relay" for e in tail))
        if scenario == "h":
            require("MTU_safe_UDP_recovered", bool(tail) and all(e["path"] == "udp" for e in tail))
        if scenario == "c":
            before = [e for e in samples if 3000 <= e["elapsed_ms"] < 6000]
            require("UDP_before_outage", bool(before) and all(e["path"] == "udp" for e in before))
            relay = [p for p in paths if p["path"] == "relay" and 6000 <= p["elapsed_ms"] <= 13000]
            require("outage_switch_under_7s", bool(relay))
            require("UDP_restored_within_70s_of_restore", bool(relay) and any(p["path"] == "udp" and
                    18000 <= p["elapsed_ms"] <= 88000 for p in paths))
            require("tail_UDP", bool(tail) and all(e["path"] == "udp" for e in tail))
        if scenario == "g":
            require("relay_before_blackhole", any(e["path"] == "relay" and 12000 <= e["elapsed_ms"] < 14000 for e in samples))
            require("UDP_after_relay_blackhole", any(e["path"] == "udp" and 14000 <= e["elapsed_ms"] <= 20000 for e in samples))
    return {
        **metrics, "assertions": checks, "failures": sorted(failures),
        "connected_ms": connected[0] if connected else None,
        "first_decoded_frame_ms": first[0] if first else None,
        "decoded_fps_over_time": fps, "paths": paths, "path_switches": len(paths) - 1,
        "session_terminations": terminations, "input_round_trips": inputs,
        "errors": [e for e in events if e["kind"] in ("failed", "terminated", "audio_error", "input_timeout")],
        "relay_connections": relay_connections,
    }


def classify(result, expected):
    if result.get("infrastructure_error"):
        return "ERROR"
    failures = result["failures"]
    if expected is None:
        return "PASS" if not failures else "FAIL"
    if any(not result.get("assertions", {}).get(name) for name in expected.get("required_assertions", [])):
        return "FAIL"
    if not failures:
        return "UNEXPECTED-PASS"
    # Quality damage varies with stochastic packet loss. The recorded bitrate
    # deficit remains mandatory, with only named, observed quality failures allowed.
    unrelated = sorted(set(failures) - set(expected.get("related_failures", [])))
    if unrelated != sorted(expected["failures"]):
        return "FAIL"
    pattern = expected.get("error_pattern")
    errors = " ".join(e.get("message", "") for e in result.get("errors", []))
    if pattern and not re.search(pattern, errors, re.I):
        return "FAIL"
    return "EXPECTED-FAILURE"


class Lab:
    def __init__(self, scenario, output):
        self.scenario = scenario
        self.output = output
        self.names = {role: f"ln{os.getpid()}{scenario}{role}" for role in ("h", "v", "c", "i")}
        self.root_links = [f"n{os.getpid()}{scenario}{end}" for end in ("x", "y")]
        self.processes = []
        self.files = []
        self.created = []
        self.etc = []
        self.log = (output / "commands.log").open("w")
        self.started = None
        self.actions = []
        self.audio_directory = None
        self.closing = False
        self.traffic = []
        self.log_offsets = {}

    def run(self, *args, input=None, check=True, timeout=15):
        if STOP.is_set() and not self.closing:
            raise InterruptedError("netlab interrupted")
        self.log.write(" ".join(map(str, args)) + "\n")
        self.log.flush()
        try:
            out = command(*args, input=input, check=check, timeout=timeout)
        except subprocess.CalledProcessError as error:
            self.log.write(error.stdout)
            self.log.flush()
            raise
        self.log.write(out)
        self.log.flush()
        return out

    def ns(self, role, *args, **kwargs):
        return self.run("ip", "netns", "exec", self.names[role], *args, **kwargs)

    def spawn(self, role, name, args, env=None):
        log = (self.output / f"{name}.log").open("a")
        self.files.append(log)
        process = subprocess.Popen(["ip", "netns", "exec", self.names[role], *map(str, args)],
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
                                   env={**os.environ, **(env or {})})
        self.processes.append(process)
        return process

    def ready(self, name, marker):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            path = self.output / f"{name}.log"
            if path.exists() and marker in path.read_text(errors="replace"):
                return
            time.sleep(0.05)
        raise RuntimeError(f"{name} did not become ready")

    def nft(self, rules):
        self.ns("c", "nft", "-f", "-", input=rules)

    def topology(self):
        for name in self.names.values():
            self.run("ip", "netns", "add", name)
            self.created.append(name)
            self.run("ip", "-n", name, "link", "set", "lo", "up")
            directory = Path("/etc/netns") / name
            directory.mkdir(parents=True)
            self.etc.append(directory)
            (directory / "hosts").write_text(f"127.0.0.1 localhost {socket.gethostname()}\n93.184.216.34 relay.netlab.test\n")
        for left, right, subnet, leftdev, rightdev in [
            ("h", "c", 1, "tail", "host"), ("c", "v", 2, "viewer", "tail"),
            ("h", "i", 3, "wan", "host"), ("c", "i", 4, "wan", "campus"),
        ]:
            # Move both ends before naming them. Kernel interface names stay under 16 bytes.
            one, two = f"n{os.getpid()}{self.scenario}x", f"n{os.getpid()}{self.scenario}y"
            self.run("ip", "link", "add", one, "type", "veth", "peer", "name", two)
            self.run("ip", "link", "set", one, "netns", self.names[left])
            self.run("ip", "link", "set", two, "netns", self.names[right])
            for role, temporary, device, suffix in [(left, one, leftdev, 1), (right, two, rightdev, 2)]:
                self.ns(role, "ip", "link", "set", temporary, "name", device)
                self.ns(role, "ip", "addr", "add", f"10.77.{subnet}.{suffix}/24", "dev", device)
                self.ns(role, "ip", "link", "set", device, "mtu", "1280", "up")
                self.ns(role, "ethtool", "-K", device, "tso", "off", "gso", "off", "gro", "off")
                features = self.ns(role, "ethtool", "-k", device)
                for feature in ("tcp-segmentation-offload", "generic-segmentation-offload", "generic-receive-offload"):
                    if not re.search(rf"^{feature}: off", features, re.M):
                        raise RuntimeError(f"{role}/{device} must disable {feature}")
        for role in self.names:
            self.ns(role, "sysctl", "-qw", "net.ipv4.ip_forward=1", "net.ipv4.conf.all.rp_filter=0",
                    "net.ipv4.conf.default.rp_filter=0", "net.ipv4.tcp_mtu_probing=0")
        self.ns("h", "ip", "addr", "add", "100.64.0.1/32", "dev", "lo")
        self.ns("v", "ip", "addr", "add", "100.64.0.2/32", "dev", "lo")
        self.ns("i", "ip", "addr", "add", "93.184.216.34/32", "dev", "lo")
        for role, dest, via, extra in [
            ("h", "100.64.0.2/32", "10.77.1.2", ["src", "100.64.0.1"]),
            ("v", "100.64.0.1/32", "10.77.2.1", ["src", "100.64.0.2"]),
            ("c", "100.64.0.1/32", "10.77.1.1", []),
            ("c", "100.64.0.2/32", "10.77.2.2", []),
            ("h", "default", "10.77.3.2", []), ("v", "default", "10.77.2.1", []),
            ("c", "default", "10.77.4.2", []), ("i", "10.77.2.0/24", "10.77.4.1", []),
        ]:
            self.ns(role, "ip", "route", "add", dest, "via", via, *extra)
        self.nft('''table inet lab {
 counter relay_dials {}
 counter udp_drops {}
 chain impairment {}
 chain forward {
  type filter hook forward priority filter; policy accept;
  ip daddr 93.184.216.34 tcp dport 443 tcp flags & (syn|ack) == syn counter name relay_dials
  jump impairment
 }
}''')
        self.ns("i", "nft", "-f", "-", input="""table inet lab {
 counter relay_dials {}
 chain input { type filter hook input priority filter; policy accept;
 tcp dport 443 tcp flags & (syn|ack) == syn counter name relay_dials
 }
}""")
        for role in ("h", "v"):
            self.ns(role, "nft", "-f", "-", input='''table inet media {
 counter udp_media {}
 counter relay_data {}
 counter relay_attempts {}
 chain input { type filter hook input priority filter; policy accept;
  ip saddr 100.64.0.0/10 udp length > 300 counter name udp_media
  tcp sport { 443, 3128 } counter name relay_data
 }
 chain output { type filter hook output priority filter; policy accept;
  tcp dport { 443, 3128 } tcp flags & (syn|ack) == syn counter name relay_attempts
 }
}''')
        self.ns("v", "ping", "-c", "1", "-W", "2", "100.64.0.1")

    def block_udp(self, blocked=True):
        self.nft("flush chain inet lab impairment\n" + (
            "add rule inet lab impairment meta l4proto udp ip saddr 100.64.0.0/10 counter name udp_drops drop\n"
            if blocked else ""))

    def certificate(self, prefix, name):
        self.run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
                 "-keyout", self.output / f"{prefix}-ca-key.pem", "-out", self.output / f"{prefix}-ca.pem",
                 "-subj", f"/CN=netlab {prefix} CA", "-addext", "basicConstraints=critical,CA:TRUE")
        self.run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", self.output / f"{prefix}-key.pem",
                 "-out", self.output / f"{prefix}.csr", "-subj", f"/CN={name}")
        extensions = self.output / f"{prefix}.ext"
        extensions.write_text(f"subjectAltName=DNS:{name}\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
        self.run("openssl", "x509", "-req", "-in", self.output / f"{prefix}.csr", "-CA", self.output / f"{prefix}-ca.pem",
                 "-CAkey", self.output / f"{prefix}-ca-key.pem", "-CAcreateserial", "-days", "2",
                 "-extfile", extensions, "-out", self.output / f"{prefix}.pem")

    def relay(self, prefix="relay"):
        return self.spawn("i", "relay", [ROOT / "target/debug/latch-relay", "--listen", "0.0.0.0:443",
            "--http-listen", "0.0.0.0:80", "--cert", self.output / f"{prefix}.pem", "--key", self.output / f"{prefix}-key.pem",
            "--quota-file", self.output / "quota", "--monthly-bytes", str(4 * 1024**3), "--initialize-quota"])

    def configure(self):
        scenario = self.scenario
        if scenario in ("b", "e", "g", "i", "i2", "j", "k", "l", "l2", "m", "m2", "n"):
            self.block_udp()
        if scenario in ("d1", "d2"):
            # c/viewer egress is the actual host -> viewer media direction.
            # Map every TOS to the clean band before explicitly selecting UDP.
            self.ns("c", "tc", "qdisc", "add", "dev", "viewer", "root", "handle", "1:",
                    "prio", "bands", "3", "priomap", *(["0"] * 16))
            if scenario == "d1":
                # Gilbert-Elliott stationary loss p/(p+r) = 17%, mean burst 2 packets.
                # Correlated random-loss percentages do not have that stationary mean.
                self.ns("c", "tc", "qdisc", "add", "dev", "viewer", "parent", "1:3", "handle", "30:",
                        "netem", "loss", "gemodel", "10.24%", "50%", "100%", "0%")
            policy = ["action", "police", "rate", "3mbit", "burst", "8k", "conform-exceed", "drop/ok"] if scenario == "d2" else []
            self.ns("c", "tc", "filter", "add", "dev", "viewer", "protocol", "ip", "parent", "1:",
                    "prio", "1", "u32", "match", "ip", "protocol", "17", "0xff", "flowid", "1:3", *policy)
        if scenario == "e":
            self.nft('''add rule inet lab impairment ip saddr 100.64.0.0/10 ip daddr 100.64.0.0/10 tcp dport 47860 accept
add rule inet lab impairment ct state established,related accept
add rule inet lab impairment iifname "viewer" tcp dport 443 accept
add rule inet lab impairment iifname "viewer" drop''')
        if scenario == "f":
            self.nft("add rule inet lab impairment ip daddr 93.184.216.34 tcp dport 443 drop")
            self.ns("i", "nft", "add", "rule", "inet", "lab", "input", "tcp", "dport", "443", "drop")
        if scenario == "h":
            self.nft("add rule inet lab impairment meta l4proto udp ip saddr 100.64.0.0/10 udp length > 600 counter name udp_drops drop")
        if scenario in ("i", "i2"):
            config = self.output / "tinyproxy.conf"
            config.write_text(f"Port 3128\nListen 10.77.2.1\nTimeout 30\nAllow 10.77.2.0/24\nConnectPort 443\nLogLevel Info\nLogFile \"{self.output}/proxy-access.log\"\nPidFile \"{self.output}/proxy.pid\"\n")
            if scenario == "i2":
                with config.open("a") as file:
                    file.write("BasicAuth lab test\n")
            self.spawn("c", "proxy", ["tinyproxy", "-d", "-c", config])
            # Native attempts reach the same HTTP-only proxy and are rejected there.
            # This gives post-preflight evidence even before CONNECT wiring exists.
            self.nft('''table ip proxy_attempts {
 chain prerouting { type nat hook prerouting priority dstnat; policy accept;
 iifname "viewer" ip daddr 93.184.216.34 tcp dport 443 redirect to :3128
 }
}''')
            self.nft('''add rule inet lab impairment ip saddr 100.64.0.0/10 ip daddr 100.64.0.0/10 tcp dport 47860 accept
add rule inet lab impairment ct state established,related accept
add rule inet lab impairment iifname "viewer" drop''')
        if scenario == "j":
            self.spawn("c", "dpi", [sys.executable, HERE / "middlebox.py", "dpi"])
            self.ready("dpi", '"kind": "ready"')
            self.nft('''table ip dnatlab {
 chain prerouting { type nat hook prerouting priority dstnat; policy accept;
 iifname "viewer" ip daddr 93.184.216.34 tcp dport 443 redirect to :8443
 }
}''')
        if scenario == "k":
            self.spawn("c", "interceptor", [sys.executable, HERE / "middlebox.py", "intercept",
                "--cert", self.output / "intercept.pem", "--key", self.output / "intercept-key.pem",
                "--ca", self.output / "relay-ca.pem"])
            self.ready("interceptor", '"kind": "ready"')
            self.nft('''table ip dnatlab {
 chain prerouting { type nat hook prerouting priority dstnat; policy accept;
 iifname "viewer" ip daddr 93.184.216.34 tcp dport 443 redirect to :8443
 }
}''')
        if scenario in ("l", "l2"):
            directory = Path("/etc/netns") / self.names["v"]
            (directory / "resolv.conf").write_text("nameserver 10.77.2.1\noptions timeout:1 attempts:1\n")
            self.spawn("c", "dns", [sys.executable, HERE / "middlebox.py", "dns"])
            self.ready("dns", '"kind": "ready"')
            self.spawn("c", "portal", [sys.executable, HERE / "middlebox.py", "portal"])
            self.ready("portal", '"kind": "ready"')
            self.nft('''table ip dnatlab {
 chain prerouting { type nat hook prerouting priority dstnat; policy accept;
 iifname "viewer" tcp dport { 80, 443 } redirect to :8080
 }
}''')
        if scenario in ("m", "m2"):
            self.nat64()
        if scenario == "n":
            # SYNs and tiny control records pass, full-sized TLS records vanish without ICMP.
            self.nft("add counter inet lab mss_drops\n"
                     "add rule inet lab impairment ip saddr 93.184.216.34 tcp sport 443 ip length > 600 counter name mss_drops drop\n"
                     "add rule inet lab impairment ip daddr 93.184.216.34 tcp dport 443 ip length > 600 counter name mss_drops drop")

    def nat64(self):
        self.nat64_prefix = "64:ff9b::/96" if self.scenario == "m" else "2001:4860:64::/96"
        for role in ("h", "v"):
            self.ns(role, "ip", "route", "del", "default")
        for role, device, addr in [("h", "tail", "fd77:1::2/64"), ("c", "host", "fd77:1::1/64"),
                                    ("v", "tail", "fd77:2::2/64"), ("c", "viewer", "fd77:2::1/64")]:
            self.ns(role, "sysctl", "-qw", "net.ipv6.conf.all.disable_ipv6=0",
                    "net.ipv6.conf.default.disable_ipv6=0", f"net.ipv6.conf.{device}.disable_ipv6=0")
            self.ns(role, "ip", "-6", "addr", "add", addr, "dev", device, "nodad")
        for role, subnet in (("h", 1), ("v", 2)):
            self.ns(role, "ip", "-6", "route", "add", "default", "via", f"fd77:{subnet}::1")
            directory = Path("/etc/netns") / self.names[role]
            (directory / "hosts").write_text("127.0.0.1 localhost\n")
            (directory / "resolv.conf").write_text(f"nameserver fd77:{subnet}::1\noptions timeout:1 attempts:1\n")
        self.ns("c", "sysctl", "-qw", "net.ipv6.conf.all.forwarding=1")
        config = self.output / "tayga.conf"
        config.write_text(f"tun-device nat64\nipv4-addr 192.0.2.1\nipv6-addr fd77:ffff::1\nprefix {self.nat64_prefix}\ndynamic-pool 192.0.2.0/24\n")
        self.ns("c", "tayga", "--config", config, "--mktun")
        self.ns("c", "ip", "link", "set", "nat64", "up")
        self.ns("c", "ip", "route", "add", "192.0.2.0/24", "dev", "nat64")
        self.ns("c", "ip", "-6", "route", "add", self.nat64_prefix, "dev", "nat64")
        self.nft('''table ip nat64 {
 chain postrouting { type nat hook postrouting priority srcnat; policy accept;
 ip saddr 192.0.2.0/24 oifname "wan" masquerade
 }
}''')
        self.spawn("c", "nat64", ["tayga", "--config", config, "--nodetach"])
        self.spawn("c", "dns64", [sys.executable, HERE / "middlebox.py", "dns64", "--prefix", self.nat64_prefix])
        self.ready("dns64", '"kind": "ready"')

    def stop_process(self, process):
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                return
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)

    def qualify_udp_loss(self):
        # Fast relay recovery ends UDP exposure before a representative loss
        # sample exists. Qualify the same qdisc with a real decoded UDP stream.
        duration = SCENARIOS["d1"][1]
        viewer = self.spawn("v", "loss-qualification", [
            ROOT / "target/debug/examples/view", "100.64.0.1", "--no-relay",
            "--width", "640", "--height", "360", "--fps", "30", "--kbps", "6000",
            "--seconds", duration, "--events", self.output / "loss-qualification.jsonl"],
            {"RUST_LOG": "warn"})
        if viewer.wait(timeout=duration + 12):
            raise RuntimeError("UDP loss qualification stream failed")
        events = read_events(self.output / "loss-qualification.jsonl")
        if not any(event.get("kind") == "sample" and event.get("decoded_seq", 0) > 0 for event in events):
            raise RuntimeError("UDP loss qualification decoded no real media")
        # Path/relay evidence and quality timings belong to the fresh session.
        # Keep qdisc counters, including the qualified real media sample.
        for role in ("c", "i"):
            self.ns(role, "nft", "reset", "counters", "table", "inet", "lab")
        for role in ("h", "v"):
            self.ns(role, "nft", "reset", "counters", "table", "inet", "media")

    def execute(self):
        self.topology()
        self.certificate("relay", "relay.netlab.test")
        if self.scenario == "k":
            self.certificate("intercept", "relay.netlab.test")
        self.relay_process = self.relay()
        self.ready("relay", "listening on")
        self.configure()
        self.preflight()
        time.sleep(0.2)  # Let completed probe handlers flush their final log entries.
        # Count only the measured endpoints, excluding preflight handshakes.
        for role in ("c", "i"):
            self.ns(role, "nft", "reset", "counters", "table", "inet", "lab")
        for role in ("h", "v"):
            self.ns(role, "nft", "reset", "counters", "table", "inet", "media")
        self.log_offsets = {path.name: path.stat().st_size for path in self.output.glob("*.log")}
        # ALSA's PulseAudio plugin connects through this private Unix socket.
        # System mode shares /run/pulse and changes UID. Give each daemon its
        # own root-owned runtime/state, independent of the runner's user session.
        self.audio_directory = tempfile.TemporaryDirectory(prefix="latch-netlab-audio-")
        audio = Path(self.audio_directory.name)
        pulse = audio / "native"
        pulse_process = self.spawn("v", "pulse", ["pulseaudio", "--system=false", "--daemonize=no", "-n", "--use-pid-file=no", "--disallow-exit", "--exit-idle-time=-1",
            "--load=module-native-protocol-unix socket=" + str(pulse) + " auth-anonymous=1",
            "--load=module-null-sink sink_name=netlab", "--log-target=stderr"],
            {"XDG_RUNTIME_DIR": str(audio), "PULSE_RUNTIME_PATH": str(audio), "PULSE_STATE_PATH": str(audio / "state")})
        deadline = time.monotonic() + 5
        while not pulse.exists() and pulse_process.poll() is None and time.monotonic() < deadline:
            time.sleep(0.05)
        if not pulse.exists():
            raise RuntimeError("private PulseAudio sink unavailable\n" + (self.output / "pulse.log").read_text())
        sinks = self.ns("v", "pactl", "--server", f"unix:{pulse}", "list", "short", "sinks")
        if not any(line.split()[1:2] == ["netlab"] for line in sinks.splitlines()):
            raise RuntimeError(f"private PulseAudio null sink missing: {sinks}")
        alsa = self.output / "asound.conf"
        alsa.write_text(f'pcm.!default {{ type pulse server "unix:{pulse}" }}\nctl.!default {{ type pulse server "unix:{pulse}" }}\n')
        common = ["--relay-host", "relay.netlab.test", "--relay-ca", self.output / "relay-ca.pem"]
        self.spawn("h", "host", [ROOT / "target/debug/examples/serve", "--bind", "100.64.0.1", "--telemetry", *common],
                   {"LATCH_TEST_PATTERN": "entropy", "LATCH_TEST_AUDIO": "clicks", "LATCH_LINUX_ENCODER": "software", "RUST_LOG": "info,latch_engine::software=debug"})
        self.ready("host", "serving on")
        if self.scenario == "d1":
            self.qualify_udp_loss()
        duration = SCENARIOS[self.scenario][1]
        self.started = time.monotonic()
        started_wall = time.time()
        viewer = self.spawn("v", "viewer", [ROOT / "target/debug/examples/view", "100.64.0.1",
            "--width", "640" if self.scenario in ("d1", "d2") else "320",
            "--height", "360" if self.scenario in ("d1", "d2") else "192",
            "--fps", "30", "--kbps", "6000", "--seconds", duration, "--audio", "--mute",
            "--events", self.output / "events.jsonl", *common],
            {"LATCH_AUDIO_PROBE": "1", "PULSE_SERVER": "unix:" + str(pulse), "LATCH_AUDIO_BUFFER": "960",
             "ALSA_CONFIG_PATH": str(alsa), "RUST_LOG": "info",
             **({"HTTPS_PROXY": "http://10.77.2.1:3128", "https_proxy": "http://10.77.2.1:3128",
                 "HTTP_PROXY": "http://10.77.2.1:3128", "NO_PROXY": "100.64.0.1"} if self.scenario in ("i", "i2") else {})})
        actions = [(6, "block_udp", lambda: self.block_udp()), (18, "restore_udp", lambda: self.block_udp(False))] if self.scenario == "c" else []
        if self.scenario == "g":
            actions = [(12, "restore_udp", lambda: self.block_udp(False)),
                       (14, "blackhole_relay", lambda: self.ns("i", "nft", "-f", "-", input=
                        "add rule inet lab input tcp sport 443 drop\n"
                        "add rule inet lab input tcp dport 443 drop\n"
                        "add chain inet lab output { type filter hook output priority filter; policy accept; }\n"
                        "add rule inet lab output tcp sport 443 drop"))]
        if self.scenario == "l2":
            actions = [(10, "open_portal", lambda: self.nft("delete table ip dnatlab"))]
        next_counter = 0
        while viewer.poll() is None:
            elapsed = time.monotonic() - self.started
            if STOP.is_set():
                raise InterruptedError("netlab interrupted")
            if elapsed > duration + 12:
                raise TimeoutError("viewer exceeded scenario deadline")
            while actions and elapsed >= actions[0][0]:
                _, name, action = actions.pop(0)
                action()
                self.actions.append({"elapsed_ms": round((time.monotonic() - self.started) * 1000), "action": name})
            if elapsed >= next_counter:
                sample = {"elapsed_ms": round(elapsed * 1000), **self.media_counters("v")}
                if self.scenario == "d2":
                    sample["police"] = police_statistics(json.loads(self.ns(
                        "c", "tc", "-s", "-j", "filter", "show", "dev", "viewer", "parent", "1:")))
                self.traffic.append(sample)
                next_counter = elapsed + 1
            time.sleep(0.05)
        if viewer.returncode:
            raise RuntimeError(f"viewer exited {viewer.returncode}")
        counters = json.loads(self.ns("i", "nft", "-j", "list", "table", "inet", "lab"))
        (self.output / "campus-nft.json").write_text(self.ns("c", "nft", "-j", "list", "ruleset"))
        (self.output / "nft.json").write_text(json.dumps(counters, indent=2))
        relay_connections = sum(item["counter"].get("packets", 0) for item in counters["nftables"]
                                if "counter" in item and item["counter"].get("name") == "relay_dials")
        result = evaluate(self.scenario, read_events(self.output / "events.jsonl"), duration, relay_connections)
        result.update(encoder_tail(self.measured_log("host.log"), started_wall, duration))
        self.verify_evidence(result, duration)
        result["actions"] = self.actions
        return result

    def preflight(self):
        # Prove the actual policy and TLS trust chain independently of Latch before measuring.
        for role in (("h", "v") if self.scenario in ("f", "m", "m2") else ("v",)):
            self.ns(role, sys.executable, HERE / "probe.py", self.scenario, self.output, role)

    def media_counters(self, role):
        counters = json.loads(self.ns(role, "nft", "-j", "list", "table", "inet", "media"))
        return {item["counter"]["name"]: {key: item["counter"].get(key, 0) for key in ("bytes", "packets")}
                for item in counters["nftables"] if "counter" in item}

    def measured_log(self, name):
        path = self.output / name
        if not path.exists():
            return ""
        with path.open("rb") as file:
            file.seek(self.log_offsets.get(name, 0))
            return file.read().decode(errors="replace")

    def verify_evidence(self, result, duration):
        def require(name, condition):
            result["assertions"][name] = bool(condition)
            if not condition:
                result["failures"].append(name)

        campus = json.loads((self.output / "campus-nft.json").read_text())
        counters = {item["counter"]["name"]: item["counter"].get("packets", 0)
                    for item in campus["nftables"] if "counter" in item}
        result["kernel_counters"] = counters
        result["kernel_traffic"] = self.traffic
        (self.output / "traffic.json").write_text(json.dumps(self.traffic, indent=2))
        tail = [sample for sample in self.traffic if (duration - 6) * 1000 <= sample["elapsed_ms"] <= duration * 1000]
        delta = {key: tail[-1][key]["bytes"] - tail[0][key]["bytes"] if len(tail) > 1 else 0
                 for key in ("udp_media", "relay_data")}
        result["tail_kernel_bytes"] = delta
        if result.get("tail_path"):
            path = result["tail_path"]
            # Five seconds of media must exceed keepalive/handshake-sized traffic.
            require("claimed_tail_path_carried_media", delta["udp_media" if path == "udp" else "relay_data"] > 10000)
        if self.scenario in ("b", "e", "g", "i", "i2", "j", "k", "l", "l2", "m", "m2", "n"):
            require("UDP_attempted_and_blocked", counters.get("udp_drops", 0) > 0)
        if self.scenario in ("c", "g"):
            relay_window = [e for e in self.traffic if 9000 <= e["elapsed_ms"] <= 14000]
            require("relay_window_carried_media", len(relay_window) > 1 and
                    relay_window[-1]["relay_data"]["bytes"] - relay_window[0]["relay_data"]["bytes"] > 10000)
            start, end = (3000, 6000) if self.scenario == "c" else (14000, 20000)
            udp_window = [e for e in self.traffic if start <= e["elapsed_ms"] <= end]
            require("UDP_transition_window_carried_media", len(udp_window) > 1 and
                    udp_window[-1]["udp_media"]["bytes"] - udp_window[0]["udp_media"]["bytes"] > 10000)
        attempts = self.media_counters("v")["relay_attempts"]["packets"]
        result["viewer_relay_attempts"] = attempts
        if self.scenario in ("a", "f"):
            require("zero_viewer_relay_attempts", attempts == 0)
            require("zero_campus_relay_dials", counters.get("relay_dials", 0) == 0)
        if self.scenario in ("i", "i2", "j", "k", "l", "l2", "n"):
            require("viewer_attempted_relay", attempts > 0)
            if self.scenario in ("i", "i2"):
                log = self.measured_log("proxy-access.log")
                reached = "Connect" in log or "CONNECT" in log
                require("proxy_CONNECT_used", "CONNECT relay.netlab.test:443" in log)
            elif self.scenario == "j":
                reached = any(e.get("kind") == "dpi" for e in
                              map(json.loads, self.measured_log("dpi.log").splitlines()))
            elif self.scenario == "k":
                reached = any(e.get("kind") in ("intercept_rejected", "intercept_accepted") for e in
                              map(json.loads, self.measured_log("interceptor.log").splitlines()))
            elif self.scenario in ("l", "l2"):
                reached = any(e.get("kind") in ("portal", "portal_connection") for e in
                              map(json.loads, self.measured_log("portal.log").splitlines()))
            else:
                reached = counters.get("mss_drops", 0) > 0
            require("viewer_reached_middlebox", reached)
        if self.scenario in ("m", "m2"):
            for role in ("h", "v"):
                endpoint = self.media_counters(role)
                result[f"{role}_kernel_counters"] = endpoint
                require(f"{role}_NAT64_relay_carried_media", endpoint["relay_data"]["bytes"] > 10000)
        if self.scenario in ("d1", "d2"):
            qdiscs = json.loads(self.ns("c", "tc", "-s", "-j", "qdisc", "show", "dev", "viewer"))
            filters = json.loads(self.ns("c", "tc", "-s", "-j", "filter", "show", "dev", "viewer", "parent", "1:"))
            (self.output / "qdiscs.json").write_text(json.dumps(qdiscs, indent=2))
            (self.output / "filters.json").write_text(json.dumps(filters, indent=2))
            if self.scenario == "d1":
                netem = next(q for q in qdiscs if q["kind"] == "netem")
                total = netem.get("packets", 0) + netem.get("drops", 0)
                loss = 100 * netem.get("drops", 0) / max(total, 1)
                result["measured_loss_pct"] = loss
                result["impaired_packets"] = total
                if total < 1000 or not 15 <= loss <= 18:
                    raise RuntimeError(f"netem must drop 15-18% of real downstream media packets, got {loss:.2f}% of {total}")
            else:
                police = police_statistics(filters)
                drops = police["drops"]
                offered_mbps = max((
                    (right["police"]["bytes"] - left["police"]["bytes"]) * 0.008 /
                    (right["elapsed_ms"] - left["elapsed_ms"])
                    for left, right in zip(self.traffic, self.traffic[1:])), default=0)
                result["policer_drops"] = drops
                result["peak_offered_udp_mbps"] = offered_mbps
                if drops <= 0 or offered_mbps <= 3:
                    raise RuntimeError(f"3 Mbps UDP policer must drop real media offered above its rate, got {drops} drops, {offered_mbps:.2f} Mbps")
        result["failures"].sort()

    def close(self):
        self.closing = True
        errors = []

        def attempt(action):
            try:
                action()
            except Exception as error:
                errors.append(str(error))

        try:
            for role in ("c", "v", "h", "i"):
                if self.names[role] in self.created:
                    for args in (("ip", "-s", "link"), ("tc", "-s", "qdisc", "show"), ("ss", "-ntup")):
                        attempt(lambda role=role, args=args: self.ns(role, *args, check=False, timeout=3))
        finally:
            try:
                for process in reversed(self.processes):
                    attempt(lambda process=process: self.stop_process(process))
            finally:
                for name in reversed(self.created):
                    attempt(lambda name=name: self.run("ip", "netns", "del", name, check=False))
                # An interruption can leave a veth pair between creation and its namespace move.
                for device in self.root_links:
                    attempt(lambda device=device: self.run("ip", "link", "del", device, check=False, timeout=3))
                for directory in self.etc:
                    attempt(lambda directory=directory: shutil.rmtree(directory))
                for file in self.files:
                    attempt(file.close)
                if self.audio_directory is not None:
                    attempt(self.audio_directory.cleanup)
                for path in self.output.glob("*-key.pem"):
                    attempt(path.unlink)
                self.log.close()
        if errors:
            raise RuntimeError("cleanup errors: " + ", ".join(errors))


def run_scenario(scenario, output, expected):
    directory = output / scenario
    directory.mkdir(parents=True, exist_ok=False)
    lab = Lab(scenario, directory)
    started = time.monotonic()
    result = {"scenario": scenario, "name": SCENARIOS[scenario][0]}
    try:
        result.update(lab.execute())
    except Exception:
        result["infrastructure_error"] = traceback.format_exc()
    finally:
        try:
            lab.close()
        except Exception:
            result["infrastructure_error"] = result.get("infrastructure_error", "") + traceback.format_exc()
    result["runtime_seconds"] = round(time.monotonic() - started, 2)
    result["expected_failure"] = expected.get(scenario)
    result["status"] = classify(result, expected.get(scenario))
    (directory / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(f"{scenario} {result['status']} {result.get('failures', [])}", flush=True)
    if result.get("infrastructure_error"):
        print(result["infrastructure_error"], flush=True)
    return result


def summary(results):
    lines = ["| Scenario | Result | First ms | Tail fps | Tail stalls >500ms | Tail freeze ms | Video/request Mbps | Encoder target Mbps | QP min/mean | Input max ms | Clicks | Tail path | Loss % | Police drops |",
             "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|"]
    for r in results:
        first = r.get("first_decoded_frame_ms", "—")
        input_ms = r.get("tail_input_max_ms")
        loss = r.get("measured_loss_pct")
        target = r.get("tail_encoder_target_mbps")
        qp = f"{r['tail_encoder_qp_min']}/{r['tail_encoder_qp_mean']:.1f}" if "tail_encoder_qp_min" in r else "—"
        lines.append(f"| {r['scenario']} {r['name']} | {r['status']} | {first if first is not None else '—'} | "
                     f"{r.get('tail_decoded_fps', 0):.1f} | {r.get('tail_decoded_stall_count', '—')} | "
                     f"{r.get('tail_max_decoded_stall_ms', '—')} | {r.get('tail_video_mbps', 0):.2f}/6 | "
                     f"{f'{target:.2f}' if target is not None else '—'} | {qp} | "
                     f"{f'{input_ms:.0f}' if input_ms is not None else '—'} | {r.get('tail_audio_clicks', '—')} | "
                     f"{r.get('tail_path', '—')} | {f'{loss:.2f}' if loss is not None else '—'} | {r.get('policer_drops', '—')} |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenarios", default=",".join(SCENARIOS))
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("--output", type=Path, default=ROOT / "target/netlab")
    args = parser.parse_args()
    if os.geteuid() != 0 or sys.platform != "linux":
        parser.error("Linux root is required. Use run-in-docker.sh on macOS.")
    scenarios = args.scenarios.split(",") if "," in args.scenarios or args.scenarios in SCENARIOS else list(args.scenarios)
    if (args.jobs not in (1, 2, 3, 4) or not scenarios or
            len(set(scenarios)) != len(scenarios) or any(s not in SCENARIOS for s in scenarios)):
        parser.error("jobs must be 1..4 and scenarios must be unique IDs separated by commas (a,b,d1,d2,...)")
    leftovers = [line for line in command("ip", "netns", "list").splitlines() if re.match(r"ln\d+", line)]
    leftovers += [str(path) for path in Path("/etc/netns").glob("ln[0-9]*")]
    if leftovers:
        parser.error("leftover netlab namespaces require cleanup before starting: " + ", ".join(leftovers))
    expected = json.loads((HERE / "expected-failures.json").read_text())
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = [pool.submit(run_scenario, s, args.output, expected) for s in scenarios]
        results = [future.result() for future in futures]
    document = {"schema": 1, "revision": os.environ.get("NETLAB_REVISION") or command("git", "-c", f"safe.directory={ROOT}", "-C", ROOT, "rev-parse", "HEAD").strip(),
                "runtime_seconds": round(time.monotonic() - started, 2), "results": results}
    (args.output / "results.json").write_text(json.dumps(document, indent=2) + "\n")
    table = summary(results)
    (args.output / "summary.md").write_text(table)
    print(table)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as file:
            file.write(table)
    return int(any(r["status"] not in ("PASS", "EXPECTED-FAILURE") for r in results))


if __name__ == "__main__":
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, lambda *_: STOP.set())
    sys.exit(main())
