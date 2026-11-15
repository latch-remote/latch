#!/usr/bin/env python3
"""Validate middlebox behavior before starting the measured Latch endpoints."""
import http.client
import json
from pathlib import Path
import socket
import ssl
import sys
import time


def main():
    scenario, directory, role = sys.argv[1:]
    directory = Path(directory)
    observations = []
    context = ssl.create_default_context(cafile=str(directory / "relay-ca.pem"))
    context.set_alpn_protocols(["latch-relay/1"])
    host = "relay.netlab.test"

    def tls(proxy=False):
        raw = socket.create_connection(("10.77.2.1", 3128) if proxy else (host, 443), timeout=2)
        with raw:
            if proxy:
                raw.sendall(b"CONNECT relay.netlab.test:443 HTTP/1.1\r\nHost: relay.netlab.test:443\r\n\r\n")
                response = b""
                while b"\r\n\r\n" not in response:
                    chunk = raw.recv(1)
                    if not chunk:
                        raise RuntimeError("CONNECT proxy closed")
                    response += chunk
                    if len(response) > 8192:
                        raise RuntimeError("CONNECT headers too large")
                assert b" 200 " in response, response
                observations.append({"connect_response": response.decode()})
            with context.wrap_socket(raw, server_hostname=host) as secure:
                assert secure.selected_alpn_protocol() == "latch-relay/1"
                observations.append({"tls": secure.version(), "alpn": secure.selected_alpn_protocol(), "peer": secure.getpeername()})

    # Services have bounded startup time. A failure still fails setup, never becomes an xfail.
    if scenario == "i":
        deadline = time.monotonic() + 5
        while True:
            try:
                tls(proxy=True)
                break
            except ConnectionRefusedError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.05)
    blocked = scenario in ("f", "i", "j", "k", "l", "n")
    try:
        tls()
        assert not blocked, "middlebox unexpectedly allowed native TLS"
    except ssl.SSLCertVerificationError as error:
        assert scenario == "k", error
        observations.append({"certificate_rejected": str(error)})
    except (OSError, ssl.SSLError) as error:
        assert blocked, error
        assert scenario != "k", "interceptor must reach certificate verification"
        observations.append({"native_tls_blocked": str(error)})
    if scenario == "l":
        connection = http.client.HTTPConnection(host, 80, timeout=2)
        connection.request("GET", "/generate_204")
        response = connection.getresponse()
        assert response.status == 302
        assert response.getheader("Location") == "http://login.netlab.test/sign-in"
        observations.append({"portal_status": response.status, "location": response.getheader("Location")})
        connection.close()
    if scenario in ("m", "m2"):
        addresses = socket.getaddrinfo(host, 443, type=socket.SOCK_STREAM)
        assert addresses and all(address[0] == socket.AF_INET6 for address in addresses), addresses
        observations.append({"dns64_addresses": [address[4][0] for address in addresses]})
    (directory / f"preflight-{role}.json").write_text(json.dumps(observations, indent=2) + "\n")


if __name__ == "__main__":
    main()
