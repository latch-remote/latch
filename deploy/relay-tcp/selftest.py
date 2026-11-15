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
import tempfile
import time

KIT = Path(__file__).resolve().parent


def check_kit():
    scripts = [KIT / name for name in ("install.sh", "network.sh", "inspect.sh", "certbot-deploy.sh")]
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
        "CapabilityBoundingSet=CAP_NET_BIND_SERVICE",
    ):
        assert setting in unit, setting
    assert "--initialize-quota" not in unit
    assert "tcp_rmem" not in (KIT / "network.sh").read_text()
    assert "tcp_wmem" not in (KIT / "network.sh").read_text()
    print("PASS kit syntax, operator limits, privilege and persistent-ledger contract")


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
            for address in ("127.0.0.1", "::1"):
                with socket.create_connection((address, port), timeout=3) as raw:
                    with tls_context(certificate).wrap_socket(raw, server_hostname="localhost") as tls:
                        assert tls.selected_alpn_protocol() == "latch-relay/1"
            for alpn in (["h2", "http/1.1"], []):
                context = tls_context(certificate)
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
    check_kit()
    if args.binary:
        check_binary(args.binary)
    if args.live:
        probe(args.live, args.port, tls_context())
        print("PASS live hostname, certificate chain and latch-relay/1 ALPN")


if __name__ == "__main__":
    main()
