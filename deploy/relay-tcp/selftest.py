#!/usr/bin/env python3
"""Read-only kit checks, optional disposable daemon checks, or a public TLS probe."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import socket
import ssl
import subprocess
import sys
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
        "MemoryMax=512M", "--listen [::]:443", "--monthly-bytes ${MONTHLY_BYTES}",
        "CapabilityBoundingSet=CAP_NET_BIND_SERVICE", "AmbientCapabilities=CAP_NET_BIND_SERVICE",
        "--http-listen [::]:80", "--acme-webroot /var/www/latch-relay",
    ):
        assert setting in unit, setting
    assert "--initialize-quota" not in unit
    network = (KIT / "network.sh").read_text()
    for setting in (
        "net.ipv4.tcp_congestion_control = bbr", "net.core.default_qdisc = fq",
        "net.core.rmem_max = 16777216", "net.core.wmem_max = 16777216",
        "net.ipv4.tcp_rmem = 4096 131072 16777216",
        "net.ipv4.tcp_wmem = 4096 65536 16777216",
        "net.ipv4.tcp_mtu_probing = 2", "net.ipv4.tcp_notsent_lowat = 131072",
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
    check_upgrade_runbook()
    check_live_gate()
    print("PASS kit syntax, operator limits, privilege and persistent-ledger contract")


def check_upgrade_runbook():
    text = (KIT / "README.md").read_text()
    upgrade = text.split("## Enable the anonymous probe and switch Certbot to webroot", 1)[1]
    blocks = re.findall(r"```bash\n(.*?)\n```", upgrade, re.S)
    assert len(blocks) == 2
    assert "inspection block below causes no relay interruption (0 seconds)" in upgrade
    assert "variable-setting block causes no relay interruption" in upgrade
    assert "For this upgrade block, plan 5–10 seconds" in upgrade
    assert "For this later rollback block, plan 5–10 seconds" in upgrade
    assert "saved `renewal.conf` points back at standalone authentication" in upgrade
    assert "port-80 probe unavailable again" in upgrade
    assert "repeat the upgrade and webroot conversion" in upgrade
    assert "checks below cause no relay interruption" in " ".join(upgrade.split())
    for block in blocks:
        subprocess.run(["bash", "-n"], input=block, text=True, check=True)
    script = blocks[0].split("bash <<'BASH'\n", 1)[1].rsplit("\nBASH", 1)[0]
    ordered = ["trap finish EXIT", "systemctl stop certbot.timer", "certbot_state=",
               'cp -a /usr/local/bin/latch-relay "$backup/latch-relay"',
               'cp -a "$renewal" "$backup/renewal.conf"', "service_stopped=1",
               'cp -a /var/lib/latch-relay/quota "$backup/quota.before"',
               'bash "$RELAY_KIT/install.sh"', 'grep -F --',
               'systemctl start latch-relay.service',
               'python3 "$RELAY_KIT/selftest.py" --live "$RELAY_HOST"',
               'for family in 4 6', 'python3 - "$renewal"', 'certbot reconfigure',
               'certbot renew --cert-name "$RELAY_CERT_NAME" --dry-run']
    # Examine the execution sequence after function definitions.
    main = script[script.index("trap finish EXIT"):]
    positions = [main.index(command) for command in ordered]
    assert positions == sorted(positions), "upgrade command order changed"
    for block in blocks:
        assert 'systemctl enable --now certbot.timer' in block
        assert 'systemctl is-active --quiet certbot.timer' in block
        assert 'cp -a "$backup/renewal.conf" "$renewal"' in block
        assert 'cp -a "$backup/latch-relay.service"' in block
        assert 'install -m 0755 "$backup/latch-relay"' in block
        assert 'cp -a "$backup/config/."' in block
        assert 'cp -a "$backup/quota.before" /var/lib/latch-relay/quota' not in block
    # The later rollback runs in its own root shell and serves the current
    # certificate, never the copy saved before the upgrade.
    later = blocks[1].split("bash <<'BASH'\n", 1)[1].rsplit("\nBASH", 1)[0]
    subprocess.run(["bash", "-n"], input=later, text=True, check=True)
    later_order = ["trap restore_timer EXIT", "systemctl stop certbot.timer",
                   'sha256sum --check "$backup/SHA256SUMS"',
                   'openssl x509 -in "$lineage/fullchain.pem" -noout -checkend 86400',
                   'openssl pkey -in "$lineage/privkey.pem" -passin pass: -check -noout',
                   'cmp -s <(openssl x509',
                   "systemctl stop latch-relay.service",
                   'cp -a /var/lib/latch-relay/quota "$backup/quota.at-rollback"',
                   'cp -a "$backup/config/." /etc/latch-relay/',
                   '"$lineage/fullchain.pem" /etc/latch-relay/fullchain.pem',
                   '"$lineage/privkey.pem" /etc/latch-relay/privkey.pem',
                   "systemctl start latch-relay.service",
                   'python3 "$RELAY_KIT/selftest.py" --live "$RELAY_HOST"',
                   'certbot renew --cert-name "$RELAY_CERT_NAME" --dry-run']
    later_main = later[later.index("trap restore_timer EXIT"):]
    later_positions = [later_main.index(command) for command in later_order]
    assert later_positions == sorted(later_positions), "later rollback order changed"
    assert ('cp -a /var/lib/latch-relay/quota "$backup/quota.at-rollback" \\\n'
            "    || echo 'The audit copy of the quota failed. Continuing with the rollback.' >&2") in later
    for name in ("fullchain.pem", "privkey.pem"):
        assert f'install -o root -g latch-relay -m 0640 "$lineage/{name}" /etc/latch-relay/{name}' in later
    for block in blocks:
        assert "trap 'exit 130' INT" in block
        assert "trap 'exit 143' HUP TERM" in block
        assert 'sudo env RELAY_HOST="$RELAY_HOST" RELAY_CERT_NAME="$RELAY_CERT_NAME"' in block
        assert 'RELAY_KIT="$PWD/deploy/relay-tcp"' in block
    assert 'RELAY_BINARY="$RELAY_BINARY" MONTHLY_BYTES="$MONTHLY_BYTES"' in blocks[0]
    assert 'backup="$backup"' in blocks[1]
    assert 'rerun' in upgrade and 'same backup' in upgrade
    assert 'avoid `set -e`' in upgrade
    assert 'no expiry e-mail' in upgrade
    assert '-checkend 2592000' in upgrade
    assert '-in /etc/latch-relay/fullchain.pem -noout -checkend 2592000' in upgrade
    assert '--live relay.' not in text
    finish = "finish() {\n" + script.split("finish() {\n", 1)[1].split("\n}\ntrap finish EXIT", 1)[0] + "\n}\n"
    with tempfile.TemporaryDirectory(prefix="latch-upgrade-exit-test-") as directory:
        trace = Path(directory) / "calls"
        for stopped, failed, rollback_failed, timer_failed in (
                (0, 0, 0, 0), (0, 1, 0, 0), (1, 1, 0, 0),
                (1, 1, 1, 0), (1, 0, 0, 1), (1, 1, 1, 1)):
            trace.write_text("")
            stub = r'''systemctl() {
    printf '%s\n' "$*" >> "$TRACE"
    if (( TIMER_FAILED )); then return 1; fi
}
rollback() {
    printf 'rollback\n' >> "$TRACE"
    return "$ROLLBACK_FAILED"
}
'''
            env = os.environ | {"TRACE": str(trace), "TIMER_FAILED": str(timer_failed),
                                "ROLLBACK_FAILED": str(rollback_failed)}
            result = subprocess.run(["bash", "-c", stub + finish
                + f"service_stopped={stopped}\ntrap finish EXIT\nexit {failed}\n"],
                env=env, capture_output=True, text=True)
            calls = trace.read_text().splitlines()
            assert calls[-2:] == ["enable --now certbot.timer", "is-active --quiet certbot.timer"], calls
            assert ("rollback" in calls) == bool(stopped and failed), calls
            assert result.returncode == int(bool(failed or timer_failed)), result.stderr
    snippets = re.findall(r"python3 - \"\$renewal\".*?<<'PY'\n(.*?)\nPY", script, re.S)
    assert len(snippets) == 2
    with tempfile.TemporaryDirectory(prefix="latch-renewal-test-") as directory:
        path = Path(directory) / "renewal.conf"
        original = ("cert = /certificate/path\nprivkey = /key/path\n"
                    "[renewalparams]\naccount = retained-account\nserver = retained-server\n"
                    "authenticator = standalone\npre_hook = systemctl stop latch-relay\n"
                    "post_hook = systemctl start latch-relay\ndeploy_hook = retained-deploy-hook\n")
        path.write_text(original)
        subprocess.run(["python3", "-", str(path)], input=snippets[0], text=True, check=True)
        hooks_removed = path.read_text()
        assert "pre_hook" not in hooks_removed and "post_hook" not in hooks_removed
        assert "retained-deploy-hook" in hooks_removed
        subprocess.run(["python3", "-", str(path), "relay.test"], input=snippets[1], text=True, check=True)
        rewritten = path.read_text()
        for retained in ("retained-account", "retained-server", "retained-deploy-hook", "/certificate/path", "/key/path"):
            assert retained in rewritten
        assert "authenticator = webroot" in rewritten
        assert "webroot_path = /var/www/latch-relay," in rewritten
        assert "[[webroot_map]]" in rewritten
        assert "relay.test = /var/www/latch-relay" in rewritten
    print("PASS upgrade/rollback syntax, timer traps, command order and renewal configuration migration")


def check_live_gate():
    # Exercise argument dispatch without running installs, daemons or live probes.
    import contextlib
    import io
    from unittest.mock import patch

    for arguments, expected in (
        ([], [("kit",)]),
        (["--live", "relay.example"], [("live", "relay.example", 443)]),
        (["--binary", "fixture"], [("kit",), ("binary", "fixture")]),
        (["--binary", "fixture", "--live", "relay.example", "--port", "8443"],
         [("kit",), ("binary", "fixture"), ("live", "relay.example", 8443)]),
    ):
        calls = []
        replacements = {
            "check_kit": lambda: calls.append(("kit",)),
            "check_binary": lambda binary: calls.append(("binary", binary)),
            "probe_carriers": lambda host, port: calls.append(("live", host, port)),
        }
        with (
            patch.dict(main.__globals__, replacements),
            patch.object(sys, "argv", [__file__, *arguments]),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            main()
        assert calls == expected, (arguments, calls)
    print("PASS live-only dispatch skips kit checks and disposable daemons")


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


def probe_websocket(host, port, context):
    context.set_alpn_protocols(["http/1.1"])
    with socket.create_connection((host, port), timeout=5) as raw:
        with context.wrap_socket(raw, server_hostname=host) as connection:
            key = base64.b64encode(os.urandom(16))
            connection.sendall(
                f"GET /relay HTTP/1.1\r\nHost: {host}:{port}\r\n".encode()
                + b"Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: "
                + key + b"\r\n\r\n")
            response = b""
            while not response.endswith(b"\r\n\r\n"):
                chunk = connection.recv(1)
                assert chunk, "WebSocket upgrade closed before its headers"
                response += chunk
                assert len(response) <= 8192, "oversized WebSocket upgrade"
            expected = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
            assert response.startswith(b"HTTP/1.1 101 "), response
            assert b"Sec-WebSocket-Accept: " + expected + b"\r\n" in response


def probe_carriers(host, port, certificate=None):
    probe(host, port, tls_context(certificate))
    probe_websocket(host, port, tls_context(certificate))


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

        webroot = directory / "webroot"
        challenge = webroot / ".well-known/acme-challenge"
        challenge.mkdir(parents=True)
        (challenge / "token_1").write_bytes(b"proof")
        (challenge / "too_large").write_bytes(b"x" * 4097)
        http_args = args.copy()
        http_args[http_args.index(str(ledger))] = str(directory / "http-quota")
        process = subprocess.Popen(http_args + ["--initialize-quota", "--http-listen", "[::]:0",
            "--acme-webroot", str(webroot)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(process.stderr, selectors.EVENT_READ)
                assert selector.select(timeout=5), "HTTP probe daemon did not start"
                line = process.stderr.readline().decode(errors="replace")
            match = re.search(r"HTTP probe on \[::\]:(\d+)", line)
            assert match, line
            http_port = int(match[1])
            for address in ("127.0.0.1", "::1"):
                for path, expected, body in (
                    ("/netenv/probe", 204, b""),
                    ("/.well-known/acme-challenge/token_1", 200, b"proof"),
                    ("/.well-known/acme-challenge/too_large", 404, b""),
                    ("/.well-known/acme-challenge/../key.pem", 404, b""),
                    ("/.well-known/acme-challenge/%2e%2e", 404, b""),
                    ("/", 404, b""),
                ):
                    with socket.create_connection((address, http_port), timeout=3) as stream:
                        stream.sendall(f"GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n".encode())
                        response = b""
                        while chunk := stream.recv(4096):
                            response += chunk
                    assert response.startswith(f"HTTP/1.1 {expected} ".encode()), response
                    assert response.split(b"\r\n\r\n", 1)[1] == body
                    assert b"Cache-Control: no-store\r\n" in response
            with socket.create_connection(("127.0.0.1", http_port), timeout=3) as stream:
                stream.sendall(b"GET /netenv/probe HTTP/1.1\r\n")
                assert stream.recv(1) == b"", "slow request did not expire"
            assert process.poll() is None
        finally:
            stop(process)
        print("PASS anonymous HTTP 204, dual stack, ACME webroot, traversal/size rejection and slow-request deadline")

        # Exercise the shipped dual-stack listener and HTTP carrier using a
        # separate ledger so the exhaustion assertions retain their small budget.
        carrier_args = args.copy()
        carrier_args[carrier_args.index("127.0.0.1:0")] = "[::]:0"
        carrier_args[carrier_args.index(str(ledger))] = str(directory / "carrier-quota")
        carrier_args[carrier_args.index("131072")] = "1048576"
        process = subprocess.Popen(carrier_args + ["--initialize-quota"], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(process.stderr, selectors.EVENT_READ)
                assert selector.select(timeout=5), "dual-stack daemon did not start"
                line = process.stderr.readline().decode(errors="replace")
            match = re.search(r"listening on \[::\]:(\d+)", line)
            assert match, line
            port = int(match[1])
            probe_carriers("localhost", port, certificate)
            for address in ("127.0.0.1", "::1"):
                with socket.create_connection((address, port), timeout=3) as raw:
                    with tls_context(certificate).wrap_socket(raw, server_hostname="localhost") as tls:
                        assert tls.selected_alpn_protocol() == "latch-relay/1"
            for alpn in (["h2", "http/1.1"], []):
                context = ssl.create_default_context(cafile=str(certificate))
                if alpn:
                    context.set_alpn_protocols(alpn)
                with socket.create_connection(("127.0.0.1", port), timeout=3) as raw:
                    with context.wrap_socket(raw, server_hostname="localhost") as tls:
                        tls.sendall(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                        response = tls.recv(1024)
                        assert response.startswith(b"HTTP/1.1 200 OK\r\n")
                        assert response.endswith(b"\r\n\r\nOK\n")
            for request, status in (
                (b"HEAD / HTTP/1.1\r\nHost: localhost\r\n\r\n", 200),
                (b"GET /missing HTTP/1.1\r\nHost: localhost\r\n\r\n", 404),
                (b"BREW / HTTP/1.1\r\nHost: localhost\r\n\r\n", 405),
                (b"GET /relay HTTP/1.1\r\nHost: localhost\r\n\r\n", 426),
                (b"invalid\r\n\r\n", 400),
                (b"GET / HTTP/1.1\r\nHost: localhost\r\nX: " + b"a" * 8192, 431),
            ):
                context = tls_context(certificate)
                context.set_alpn_protocols(["http/1.1"])
                with socket.create_connection(("127.0.0.1", port), timeout=3) as raw:
                    with context.wrap_socket(raw, server_hostname="localhost") as tls:
                        tls.sendall(request)
                        response = tls.recv(1024)
                        assert response.startswith(f"HTTP/1.1 {status} ".encode()), response
                        if request.startswith(b"HEAD "):
                            assert response.endswith(b"\r\n\r\n")
                        if status == 426:
                            assert b"Sec-WebSocket-Version: 13\r\n" in response
                        assert b"latch" not in response.lower()
            context = tls_context(certificate)
            context.set_alpn_protocols(["h2", "http/1.1"])
            with socket.create_connection(("::1", port), timeout=3) as raw:
                with context.wrap_socket(raw, server_hostname="localhost") as tls:
                    key = base64.b64encode(os.urandom(16))
                    tls.sendall(b"GET /relay HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: " + key + b"\r\n\r\n")
                    response = b""
                    while not response.endswith(b"\r\n\r\n"):
                        response += tls.recv(1)
                        assert len(response) < 8192
                    expected = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
                    assert response.startswith(b"HTTP/1.1 101 ") and expected in response
                    mask = os.urandom(4)
                    record = b"\x00\x00\x00\x01\x01"
                    tls.sendall(b"\x82\x85" + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(record)))
                    def read_exact(size):
                        result = b""
                        while len(result) < size:
                            chunk = tls.recv(size - len(result))
                            assert chunk, "truncated WebSocket grant"
                            result += chunk
                        return result
                    assert read_exact(2) == b"\x82\x7e"
                    assert int.from_bytes(read_exact(2), "big") == 185
                    grant = read_exact(185)
                    assert grant[:5] == b"\x00\x00\x00\xb5\x01"
        finally:
            stop(process)
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
    print("PASS real daemon dual-stack, TLS/ALPN, HTTP, WebSocket, pilot JSON, private ledger, lock, restart, exhaustion and corruption")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", help="run disposable loopback tests against this native binary")
    parser.add_argument("--live", metavar="HOSTNAME", help="verify public WebPKI TLS and relay ALPN")
    parser.add_argument("--port", type=int, default=443)
    args = parser.parse_args()
    # A live check runs as root on the production relay. It must not also run
    # the kit's simulated installs there, however well they are redirected.
    if not args.live or args.binary:
        check_kit()
    if args.binary:
        check_binary(args.binary)
    if args.live:
        probe_carriers(args.live, args.port)
        print("PASS live hostname, certificate chain, native TLS and WebSocket upgrade")


if __name__ == "__main__":
    main()
