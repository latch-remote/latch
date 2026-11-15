#!/usr/bin/env python3
"""Exercise real Latch binaries in disposable Linux network namespaces."""
import argparse
import concurrent.futures
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
import time
import traceback

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
SCENARIOS = {
    "a": ("clean UDP", 18),
    "b": ("UDP denied at startup", 22),
    "c": ("UDP outage and restoration", 85),
    "d": ("17% burst loss, jitter, 3 Mbps UDP", 30),
    "e": ("campus TCP 443 only", 22),
    "f": ("unreachable relay, healthy UDP", 18),
    "g": ("relay dies during stream", 32),
    "h": ("UDP MTU black hole", 25),
    "i": ("explicit CONNECT proxy only", 22),
    "j": ("HTTPS-only ALPN inspector", 22),
    "k": ("TLS interception", 16),
    "l": ("captive portal", 16),
    "m": ("IPv6-only internet with DNS64/NAT64", 25),
    "n": ("relay TCP MSS black hole", 26),
}


def command(*args, input=None, check=True):
    return subprocess.run(list(map(str, args)), input=input, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                          timeout=15, check=check).stdout


def read_events(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


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
    if scenario not in ("k", "l"):
        require("no_session_termination", not terminations)
    require("sampler_completed", bool(samples) and samples[-1]["elapsed_ms"] >= (duration - 1) * 1000)
    if scenario in ("k", "l"):
        require("no_untrusted_media", not first)
        expression = r"certificate|unknown.?issuer|tls.{0,30}(trust|intercept)" if scenario == "k" else r"captive|portal|sign.in"
        require("clear_security_error_under_8s", any(
            re.search(expression, e.get("message", ""), re.I) and e["elapsed_ms"] <= 8000 for e in events))
    else:
        first_limit = 3000 if scenario in ("a", "f") else 7000
        if scenario in ("h", "n"):
            first_limit = 4000
        require("first_frame_deadline", bool(first) and first[0] <= first_limit)
        tail = [e for e in samples if e["elapsed_ms"] >= (duration - 6) * 1000]
        tail_fps = ((tail[-1]["decoded_seq"] - tail[0]["decoded_seq"]) * 1000 /
                    (tail[-1]["elapsed_ms"] - tail[0]["elapsed_ms"])) if len(tail) > 1 else 0
        require("tail_decoded_fps_at_least_15", tail_fps >= 15)
        progress = []
        previous = 0
        for e in samples:
            if e["decoded_seq"] > previous:
                progress.append(e["elapsed_ms"])
                previous = e["decoded_seq"]
        maximum_gap = max([b - a for a, b in zip(progress, progress[1:])] +
                          ([samples[-1]["elapsed_ms"] - progress[-1]] if progress and samples else [duration * 1000]))
        require("no_decoded_stall_over_5s", bool(progress) and maximum_gap <= 5000)
        late_inputs = [e for e in inputs if e["elapsed_ms"] >= (duration - 7) * 1000]
        require("tail_input_round_trips", len(late_inputs) >= 3 and max(e["round_trip_ms"] for e in late_inputs) <= 1500)
        clicks = [value for e in samples for value in e.get("audio_click_ms", []) if e["elapsed_ms"] >= (duration - 7) * 1000]
        require("tail_audio_clicks", len(clicks) >= 4)
        metrics = {"tail_decoded_fps": tail_fps, "max_decoded_stall_ms": maximum_gap,
                   "tail_input_round_trips": len(late_inputs), "tail_audio_clicks": len(clicks)}
        if scenario in ("a", "f"):
            require("UDP_only", all(p["path"] == "udp" for p in paths))
            require("zero_relay_connections", relay_connections == 0)
        if scenario in ("b", "e", "i", "j", "m", "n"):
            require("relay_active", bool(tail) and all(e["path"] == "relay" for e in tail))
        if scenario == "h":
            require("MTU_safe_UDP_recovered", bool(tail) and all(e["path"] == "udp" for e in tail))
        if scenario == "c":
            before = [e for e in samples if 3000 <= e["elapsed_ms"] < 6000]
            require("UDP_before_outage", bool(before) and all(e["path"] == "udp" for e in before))
            relay = [p for p in paths if p["path"] == "relay" and 6000 <= p["elapsed_ms"] <= 13000]
            require("outage_switch_under_7s", bool(relay))
            require("UDP_restored_under_70s", bool(relay) and any(p["path"] == "udp" and
                    18000 <= p["elapsed_ms"] <= 88000 for p in paths))
            require("tail_UDP", bool(tail) and all(e["path"] == "udp" for e in tail))
        if scenario == "g":
            require("relay_before_kill", any(e["path"] == "relay" and 12000 <= e["elapsed_ms"] < 14000 for e in samples))
            require("UDP_after_relay_death", any(e["path"] == "udp" and 14000 <= e["elapsed_ms"] <= 20000 for e in samples))
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
    if not failures:
        return "UNEXPECTED-PASS"
    if failures != sorted(expected["failures"]):
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
        self.processes = []
        self.files = []
        self.created = []
        self.etc = []
        self.log = (output / "commands.log").open("w")
        self.started = None
        self.actions = []
        self.audio_directory = None

    def run(self, *args, input=None, check=True):
        self.log.write(" ".join(map(str, args)) + "\n")
        self.log.flush()
        try:
            out = command(*args, input=input, check=check)
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
            (directory / "hosts").write_text(f"127.0.0.1 localhost {socket.gethostname()}\n198.18.0.1 relay.netlab.test\n")
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
        for role in self.names:
            self.ns(role, "sysctl", "-qw", "net.ipv4.ip_forward=1", "net.ipv4.conf.all.rp_filter=0",
                    "net.ipv4.conf.default.rp_filter=0", "net.ipv4.tcp_mtu_probing=0")
        self.ns("h", "ip", "addr", "add", "100.64.0.1/32", "dev", "lo")
        self.ns("v", "ip", "addr", "add", "100.64.0.2/32", "dev", "lo")
        self.ns("i", "ip", "addr", "add", "198.18.0.1/32", "dev", "lo")
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
  ip daddr 198.18.0.1 tcp dport 443 tcp flags & (syn|ack) == syn counter name relay_dials
  jump impairment
 }
}''')
        self.ns("i", "nft", "-f", "-", input="""table inet lab {
 counter relay_dials {}
 chain input { type filter hook input priority filter; policy accept;
 tcp dport 443 tcp flags & (syn|ack) == syn counter name relay_dials
 }
}""")
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
            "--cert", self.output / f"{prefix}.pem", "--key", self.output / f"{prefix}-key.pem",
            "--quota-file", self.output / "quota", "--monthly-bytes", str(4 * 1024**3), "--initialize-quota"])

    def configure(self):
        scenario = self.scenario
        if scenario in ("b", "e", "g", "i", "j", "k", "l", "m", "n"):
            self.block_udp()
        if scenario == "d":
            # Shape only host-origin media, so control TCP and relay TCP remain independent.
            self.ns("c", "tc", "qdisc", "add", "dev", "viewer", "root", "handle", "1:", "prio")
            self.ns("c", "tc", "qdisc", "add", "dev", "viewer", "parent", "1:3", "handle", "30:",
                    "netem", "delay", "25ms", "10ms", "25%", "loss", "17%", "35%")
            self.ns("c", "tc", "qdisc", "add", "dev", "viewer", "parent", "30:1", "handle", "40:",
                    "tbf", "rate", "3mbit", "burst", "16kb", "latency", "80ms")
            self.ns("c", "tc", "filter", "add", "dev", "viewer", "protocol", "ip", "parent", "1:",
                    "prio", "1", "u32", "match", "ip", "protocol", "17", "0xff", "flowid", "1:3")
        if scenario == "e":
            self.nft('''add rule inet lab impairment ip saddr 100.64.0.0/10 ip daddr 100.64.0.0/10 tcp dport 47860 accept
add rule inet lab impairment ct state established,related accept
add rule inet lab impairment iifname "viewer" tcp dport 443 accept
add rule inet lab impairment iifname "viewer" drop''')
        if scenario == "f":
            self.nft("add rule inet lab impairment ip daddr 198.18.0.1 tcp dport 443 drop")
        if scenario == "h":
            self.nft("add rule inet lab impairment meta l4proto udp ip saddr 100.64.0.0/10 udp length > 600 counter name udp_drops drop")
        if scenario == "i":
            config = self.output / "tinyproxy.conf"
            config.write_text(f"Port 3128\nListen 10.77.2.1\nTimeout 30\nAllow 10.77.2.0/24\nConnectPort 443\nLogLevel Info\nLogFile \"{self.output}/proxy-access.log\"\nPidFile \"{self.output}/proxy.pid\"\n")
            self.spawn("c", "proxy", ["tinyproxy", "-d", "-c", config])
            self.nft('''add rule inet lab impairment ip saddr 100.64.0.0/10 ip daddr 100.64.0.0/10 tcp dport 47860 accept
add rule inet lab impairment ct state established,related accept
add rule inet lab impairment iifname "viewer" drop''')
        if scenario == "j":
            self.spawn("c", "dpi", [sys.executable, HERE / "middlebox.py", "dpi"])
            self.ready("dpi", '"kind": "ready"')
            self.nft('''table ip dnatlab {
 chain prerouting { type nat hook prerouting priority dstnat; policy accept;
 iifname "viewer" ip daddr 198.18.0.1 tcp dport 443 redirect to :8443
 }
}''')
        if scenario == "k":
            self.spawn("c", "interceptor", [sys.executable, HERE / "middlebox.py", "intercept",
                "--cert", self.output / "intercept.pem", "--key", self.output / "intercept-key.pem",
                "--ca", self.output / "relay-ca.pem"])
            self.ready("interceptor", '"kind": "ready"')
            self.nft('''table ip dnatlab {
 chain prerouting { type nat hook prerouting priority dstnat; policy accept;
 iifname "viewer" ip daddr 198.18.0.1 tcp dport 443 redirect to :8443
 }
}''')
        if scenario == "l":
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
        if scenario == "m":
            self.nat64()
        if scenario == "n":
            # SYNs and tiny control records pass, full-sized TLS records vanish without ICMP.
            self.nft("add rule inet lab impairment ip saddr 198.18.0.1 tcp sport 443 ip length > 600 drop\n"
                     "add rule inet lab impairment ip daddr 198.18.0.1 tcp dport 443 ip length > 600 drop")

    def nat64(self):
        self.ns("v", "ip", "route", "del", "default")
        for role, device, addr in [("v", "tail", "fd77:2::2/64"), ("c", "viewer", "fd77:2::1/64")]:
            self.ns(role, "sysctl", "-qw", "net.ipv6.conf.all.disable_ipv6=0",
                    "net.ipv6.conf.default.disable_ipv6=0", f"net.ipv6.conf.{device}.disable_ipv6=0")
            self.ns(role, "ip", "-6", "addr", "add", addr, "dev", device, "nodad")
        self.ns("v", "ip", "-6", "route", "add", "default", "via", "fd77:2::1")
        self.ns("c", "sysctl", "-qw", "net.ipv6.conf.all.forwarding=1")
        config = self.output / "tayga.conf"
        config.write_text("tun-device nat64\nipv4-addr 192.0.2.1\nipv6-addr fd77:ffff::1\nprefix fd77:64::/96\ndynamic-pool 192.0.2.0/24\n")
        self.ns("c", "tayga", "--config", config, "--mktun")
        self.ns("c", "ip", "link", "set", "nat64", "up")
        self.ns("c", "ip", "route", "add", "192.0.2.0/24", "dev", "nat64")
        self.ns("c", "ip", "-6", "route", "add", "fd77:64::/96", "dev", "nat64")
        self.nft('''table ip nat64 {
 chain postrouting { type nat hook postrouting priority srcnat; policy accept;
 ip saddr 192.0.2.0/24 oifname "wan" masquerade
 }
}''')
        directory = Path("/etc/netns") / self.names["v"]
        (directory / "hosts").write_text("127.0.0.1 localhost\n")
        (directory / "resolv.conf").write_text("nameserver fd77:2::1\noptions timeout:1 attempts:1\n")
        self.spawn("c", "nat64", ["tayga", "--config", config, "--nodetach"])
        self.spawn("c", "dns64", [sys.executable, HERE / "middlebox.py", "dns64"])
        self.ready("dns64", '"kind": "ready"')

    def stop_process(self, process):
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)

    def execute(self):
        self.topology()
        self.certificate("relay", "relay.netlab.test")
        if self.scenario == "k":
            self.certificate("intercept", "relay.netlab.test")
        self.relay_process = self.relay()
        self.ready("relay", "listening on")
        self.configure()
        self.preflight()
        # Count only the measured endpoints, excluding preflight handshakes.
        self.ns("i", "nft", "reset", "counter", "inet", "lab", "relay_dials")
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
                   {"LATCH_TEST_PATTERN": "noise" if self.scenario == "d" else "sweep", "LATCH_TEST_AUDIO": "clicks", "LATCH_LINUX_ENCODER": "software", "RUST_LOG": "info"})
        self.ready("host", "serving on")
        duration = SCENARIOS[self.scenario][1]
        self.started = time.monotonic()
        viewer = self.spawn("v", "viewer", [ROOT / "target/debug/examples/view", "100.64.0.1", "--width", "320",
            "--height", "192", "--fps", "30", "--kbps", "6000", "--seconds", duration, "--audio", "--mute",
            "--events", self.output / "events.jsonl", *common],
            {"LATCH_AUDIO_PROBE": "1", "PULSE_SERVER": "unix:" + str(pulse), "ALSA_CONFIG_PATH": str(alsa), "RUST_LOG": "info",
             **({"HTTPS_PROXY": "http://10.77.2.1:3128", "https_proxy": "http://10.77.2.1:3128",
                 "HTTP_PROXY": "http://10.77.2.1:3128", "NO_PROXY": "100.64.0.1"} if self.scenario == "i" else {})})
        actions = [(6, "block_udp", lambda: self.block_udp()), (18, "restore_udp", lambda: self.block_udp(False))] if self.scenario == "c" else []
        if self.scenario == "g":
            actions = [(12, "restore_udp", lambda: self.block_udp(False)),
                       (14, "kill_relay", lambda: self.stop_process(self.relay_process))]
        for at, name, action in actions:
            while time.monotonic() - self.started < at:
                if viewer.poll() is not None:
                    raise RuntimeError("viewer exited before scheduled impairment")
                time.sleep(0.05)
            action()
            self.actions.append({"elapsed_ms": round((time.monotonic() - self.started) * 1000), "action": name})
        viewer.wait(timeout=duration + 12 - (time.monotonic() - self.started))
        if viewer.returncode:
            raise RuntimeError(f"viewer exited {viewer.returncode}")
        counters = json.loads(self.ns("i", "nft", "-j", "list", "table", "inet", "lab"))
        (self.output / "campus-nft.json").write_text(self.ns("c", "nft", "-j", "list", "ruleset"))
        (self.output / "nft.json").write_text(json.dumps(counters, indent=2))
        relay_connections = sum(item["counter"].get("packets", 0) for item in counters["nftables"]
                                if "counter" in item and item["counter"].get("name") == "relay_dials")
        result = evaluate(self.scenario, read_events(self.output / "events.jsonl"), duration, relay_connections)
        if self.scenario == "d":
            qdiscs = json.loads(self.ns("c", "tc", "-s", "-j", "qdisc", "show", "dev", "viewer"))
            (self.output / "qdiscs.json").write_text(json.dumps(qdiscs, indent=2))
            netem = next(q for q in qdiscs if q["kind"] == "netem")
            tbf = next(q for q in qdiscs if q["kind"] == "tbf")
            if netem.get("drops", 0) == 0 or tbf.get("overlimits", 0) == 0:
                raise RuntimeError("UDP loss and policer must both carry and impair real media")
        result["actions"] = self.actions
        return result

    def preflight(self):
        if self.scenario == "m":
            self.ns("v", "ip", "-6", "addr", "show")
            self.ns("v", "ip", "-6", "route", "show")
            self.ns("v", "ping", "-6", "-c", "1", "-W", "2", "fd77:64::c612:1")
        # Prove the actual policy and TLS trust chain independently of Latch before measuring.
        probe = HERE / "probe.py"
        self.ns("v", sys.executable, probe, self.scenario, self.output)

    def close(self):
        for role in ("c", "v", "h", "i"):
            if self.names[role] in self.created:
                self.ns(role, "ip", "-s", "link", check=False)
                self.ns(role, "tc", "-s", "qdisc", "show", check=False)
                self.ns(role, "ss", "-ntup", check=False)
        for process in reversed(self.processes):
            self.stop_process(process)
        for file in self.files:
            file.close()
        if self.audio_directory is not None:
            self.audio_directory.cleanup()
        for name in reversed(self.created):
            self.run("ip", "netns", "del", name, check=False)
        for directory in self.etc:
            shutil.rmtree(directory)
        # Test-only private keys are disposable and excluded from artifacts.
        for path in self.output.glob("*-key.pem"):
            path.unlink()
        self.log.close()


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
    lines = ["| Scenario | Result | Connect ms | First frame ms | Tail decoded fps | Switches |", "|---|---|---:|---:|---:|---:|"]
    for r in results:
        connect = r.get("connected_ms")
        first = r.get("first_decoded_frame_ms")
        lines.append(f"| {r['scenario']} {r['name']} | {r['status']} | {connect if connect is not None else '—'} | "
                     f"{first if first is not None else '—'} | {r.get('tail_decoded_fps', 0):.1f} | {r.get('path_switches', '—')} |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenarios", default="".join(SCENARIOS))
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("--output", type=Path, default=ROOT / "target/netlab")
    args = parser.parse_args()
    if os.geteuid() != 0 or sys.platform != "linux":
        parser.error("Linux root is required. Use run-in-docker.sh on macOS.")
    if (args.jobs not in (1, 2, 3, 4) or not args.scenarios or
            len(set(args.scenarios)) != len(args.scenarios) or any(s not in SCENARIOS for s in args.scenarios)):
        parser.error("jobs must be 1..4 and scenarios must be unique letters a..n")
    expected = json.loads((HERE / "expected-failures.json").read_text())
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = [pool.submit(run_scenario, s, args.output, expected) for s in args.scenarios]
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
    sys.exit(main())
