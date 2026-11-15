#!/usr/bin/env python3
"""Read-only kit checks, optional disposable daemon checks, or a public TLS probe."""
import argparse
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import socket
import ssl
import subprocess
import tempfile
import time

KIT = Path(__file__).resolve().parent


def check_kit():
    scripts = [KIT / name for name in (
        "install.sh", "network.sh", "tune-host.sh", "inspect.sh", "certbot-deploy.sh",
    )]
    for script in scripts:
        subprocess.run(["bash", "-n", str(script)], check=True)
    if shutil.which("shellcheck"):
        subprocess.run(["shellcheck", *map(str, scripts)], check=True)
    limits = json.loads((KIT / "limits.json").read_text())
    assert limits["max_connections"] == 24
    assert limits["max_sessions"] == 4
    assert limits["session_bytes"] == 16 * 1024 * 1024 * 1024
    assert limits["session_lifetime"] == {"secs": 3600, "nanos": 0}
    unit = (KIT / "latch-relay.service").read_text()
    for setting in (
        "User=latch-relay", "NoNewPrivileges=yes", "ProtectSystem=strict",
        "StateDirectory=latch-relay", "Restart=always", "StartLimitIntervalSec=0",
        "MemoryMax=512M", "--listen 0.0.0.0:443", "--monthly-bytes ${MONTHLY_BYTES}",
        "CapabilityBoundingSet=CAP_NET_BIND_SERVICE",
    ):
        assert setting in unit, setting
    assert "--initialize-quota" not in unit
    network = (KIT / "network.sh").read_text()
    for setting in (
        "net.ipv4.tcp_congestion_control = bbr", "net.core.default_qdisc = fq",
        "net.core.rmem_max = 16777216", "net.core.wmem_max = 16777216",
        "net.ipv4.tcp_rmem = 4096 131072 16777216",
        "net.ipv4.tcp_wmem = 4096 65536 16777216",
        "net.ipv4.tcp_mtu_probing = 1", "net.ipv4.tcp_notsent_lowat = 131072",
        "net.ipv4.tcp_slow_start_after_idle = 0",
        "net.netfilter.nf_conntrack_max = 131072",
        "rx-udp-gro-forwarding on rx-gro-list off", "LimitNOFILE=65536",
        "MemoryHigh=384M", "MemoryMax=512M",
    ):
        assert setting in network, setting
    assert not re.search(
        r"systemctl (?:restart|stop) (?:latch-relay|tailscaled)(?:\.service)?(?:\s|$)",
        network,
    )
    check_network(network)
    print("PASS kit syntax, operator limits, privilege and persistent-ledger contract")


def check_network(network):
    """Run the installer and model an early boot with conntrack initially absent."""
    with tempfile.TemporaryDirectory(prefix="latch-network-selftest-") as directory:
        root = Path(directory).resolve()
        commands = root / "bin"
        commands.mkdir()
        stub = commands / "stub"
        stub.write_text('''#!/usr/bin/env python3
import os
from pathlib import Path
import sys

root = Path(os.environ["NETWORK_TEST_ROOT"])
command = Path(sys.argv[0]).name
args = sys.argv[1:]
module = root / "module-loaded"
with (root / "calls").open("a") as log:
    log.write(command + " " + " ".join(args) + "\\n")
if command == "id":
    print(0)
elif command == "uname":
    print("Linux")
elif command == "readlink":
    print(Path(args[-1]).resolve())
elif command == "modprobe" and args == ["nf_conntrack"]:
    if os.environ.get("FAIL_CONNTRACK"):
        sys.exit(1)
    module.touch()
elif command == "sysctl":
    if args == ["-n", "net.ipv4.tcp_available_congestion_control"]:
        print("reno cubic bbr")
    elif args[0] == "-p":
        values = Path(args[1]).read_text()
        if "net.netfilter.nf_conntrack_max" in values and not module.exists():
            sys.exit(1)
        (root / "effective-sysctls").write_text(values)
    else:
        sys.exit(1)
elif command == "systemctl" and args[0] == "show":
    print(0)
elif command == "ip":
    print("default via 192.0.2.1 dev test0")
''')
        stub.chmod(0o755)
        for command in (
            "id", "uname", "readlink", "modprobe", "sysctl", "systemctl",
            "ip", "ethtool", "prlimit", "tc",
        ):
            (commands / command).symlink_to(stub)
        # Redirect filesystem writes while executing the actual shell logic.
        script = root / "network.sh"
        for prefix in ("/etc/", "/usr/local/", "/proc/"):
            network = network.replace(prefix, str(root) + prefix)
        script.write_text(network)
        (root / "usr/local/sbin").mkdir(parents=True)
        env = dict(os.environ, PATH=str(commands) + os.pathsep + os.environ["PATH"],
                   NETWORK_TEST_ROOT=str(root))

        def run_script(path, **extra):
            return subprocess.run(["bash", str(path)], env=dict(env, **extra),
                                  capture_output=True, text=True)

        failed = run_script(script, FAIL_CONNTRACK="1")
        assert failed.returncode != 0
        assert not (root / "etc").exists(), "module failure left partial configuration"
        (root / "etc/systemd/system").mkdir(parents=True)
        result = run_script(script)
        assert result.returncode == 0, result.stderr
        installed = root / "usr/local/sbin/latch-relay-network"
        result = run_script(installed)
        assert result.returncode == 0, result.stderr
        calls = (root / "calls").read_text().splitlines()
        load = calls.index("modprobe nf_conntrack", 1)
        apply = next(i for i, call in enumerate(calls) if call.startswith("sysctl -p"))
        assert load < apply, "sysctls applied before loading conntrack"

        # systemd-modules-load precedes systemd-sysctl at boot. Recreate that
        # sequence from the installed files, without running the installer.
        (root / "module-loaded").unlink()
        (root / "effective-sysctls").unlink()
        modules = (root / "etc/modules-load.d/latch-relay.conf").read_text().splitlines()
        for module in modules:
            subprocess.run([str(commands / "modprobe"), module], env=env, check=True)
        subprocess.run([str(commands / "sysctl"), "-p",
                        str(root / "etc/sysctl.d/90-latch-relay.conf")], env=env, check=True)
        assert "net.netfilter.nf_conntrack_max = 131072" in (
            root / "effective-sysctls").read_text(), "conntrack ceiling lost at boot"
        result = subprocess.run(["bash", str(installed), "--offloads-only"],
                                env=env, capture_output=True, text=True)
        assert result.returncode == 0, result.stderr
    print("PASS network install, missing-module failure, installed re-run and boot persistence")


def tls_context(certificate=None):
    context = ssl.create_default_context(cafile=str(certificate) if certificate else None)
    context.set_alpn_protocols(["latch-relay/1"])
    return context


def probe(host, port, context):
    with socket.create_connection((host, port), timeout=5) as raw:
        with context.wrap_socket(raw, server_hostname=host) as connection:
            assert connection.selected_alpn_protocol() == "latch-relay/1"
            assert connection.version() in ("TLSv1.2", "TLSv1.3")


def check_binary(binary):
    binary = str(Path(binary).resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix="latch-relay-selftest-") as directory:
        directory = Path(directory)
        key = directory / "key.pem"
        certificate = directory / "cert.pem"
        ledger = directory / "quota"
        subprocess.run([
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(key), "-out", str(certificate), "-days", "1",
            "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost",
        ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        os.chmod(key, 0o600)
        args = [
            binary, "--listen", "127.0.0.1:0", "--cert", str(certificate),
            "--key", str(key), "--quota-file", str(ledger),
            "--monthly-bytes", "131072", "--limits-json", str(KIT / "limits.json"),
        ]

        def must_fail(extra=()):
            result = subprocess.run(args + list(extra), capture_output=True, timeout=5)
            assert result.returncode != 0, "daemon unexpectedly accepted invalid state"

        def start(extra=()):
            process = subprocess.Popen(args + list(extra), stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(process.stderr, selectors.EVENT_READ)
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        if not selector.select(timeout=0.1):
                            continue
                        line = process.stderr.readline().decode(errors="replace")
                        match = re.search(r"listening on 127\.0\.0\.1:(\d+)", line)
                        if match:
                            return process, int(match[1])
                        if not line or process.poll() is not None:
                            break
                raise AssertionError("daemon failed to bind with the current CLI and CPU")
            except BaseException:
                stop(process)
                raise

        def stop(process):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
            process.stderr.close()

        must_fail()
        assert not ledger.exists(), "ordinary startup recreated a missing ledger"
        process, port = start(["--initialize-quota"])
        try:
            assert ledger.stat().st_size == 32
            assert ledger.stat().st_mode & 0o777 == 0o600
            must_fail()  # A second process cannot share the locked ledger.
            probe("localhost", port, tls_context(certificate))
        finally:
            stop(process)
        before = ledger.read_bytes()
        assert int.from_bytes(before[16:24], "big") > 0
        must_fail(["--initialize-quota"])
        assert ledger.read_bytes() == before, "initialization replaced an existing ledger"
        process, port = start()
        try:
            assert ledger.read_bytes() == before, "restart reset reserved bytes"
            # Exhaustion rejects fresh TLS admission while keeping the daemon
            # alive for existing credit and the next monthly rollover.
            rejected = False
            for _ in range(4):
                try:
                    probe("localhost", port, tls_context(certificate))
                except (OSError, ssl.SSLError):
                    rejected = True
                    break
            assert rejected, "exhausted budget admitted another TLS handshake"
            # EOF can arrive before the CLI finishes joining its logger. Give
            # a fatal admission path time to exit before asserting liveness.
            time.sleep(0.5)
            assert process.poll() is None, "admission exhaustion terminated the daemon"
        finally:
            stop(process)
        reserved = int.from_bytes(ledger.read_bytes()[16:24], "big")
        assert 0 < reserved <= 131072
        exhausted = ledger.read_bytes()
        process, port = start()
        try:
            try:
                probe("localhost", port, tls_context(certificate))
                raise AssertionError("restart reopened an exhausted budget")
            except (OSError, ssl.SSLError):
                pass
            time.sleep(0.5)
            assert process.poll() is None, "exhausted restart terminated after admission"
            assert ledger.read_bytes() == exhausted, "rejected admission changed the exhausted ledger"
        finally:
            stop(process)
        ledger.write_bytes(b"damaged")
        must_fail()
        assert ledger.read_bytes() == b"damaged", "corruption was silently reset"
    print("PASS real daemon TLS/ALPN, pilot JSON, private ledger, lock, restart, exhaustion and corruption")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", help="run disposable loopback tests against this native binary")
    parser.add_argument("--live", metavar="HOSTNAME", help="verify public WebPKI TLS and relay ALPN")
    parser.add_argument("--port", type=int, default=443)
    args = parser.parse_args()
    check_kit()
    if args.binary:
        check_binary(args.binary)
    if args.live:
        probe(args.live, args.port, tls_context())
        print("PASS live hostname, certificate chain and latch-relay/1 ALPN")


if __name__ == "__main__":
    main()
