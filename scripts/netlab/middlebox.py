#!/usr/bin/env python3
"""Disposable DNS64, ALPN-inspecting bridge and portal for isolated netlab namespaces."""
import argparse
import http.server
import ipaddress
import json
import select
import socket
import socketserver
import ssl
import struct
import time


def log(**fields):
    print(json.dumps({"time": time.time(), **fields}), flush=True)


def alpns(hello):
    """Parse the TLS ClientHello extensions without terminating TLS."""
    body = memoryview(hello)[5:]
    if len(body) < 39 or body[0] != 1:
        raise ValueError("not a ClientHello")
    offset = 38
    offset += 1 + body[offset]
    offset += 2 + int.from_bytes(body[offset:offset + 2], "big")
    offset += 1 + body[offset]
    end = offset + 2 + int.from_bytes(body[offset:offset + 2], "big")
    offset += 2
    while offset + 4 <= end:
        kind, size = struct.unpack_from("!HH", body, offset)
        offset += 4
        extension = body[offset:offset + size]
        offset += size
        if kind == 16:
            protocols = []
            index = 2
            while index < len(extension):
                length = extension[index]
                index += 1
                protocols.append(bytes(extension[index:index + length]).decode("ascii"))
                index += length
            return protocols
    return []


class DPI(socketserver.BaseRequestHandler):
    def handle(self):
        incoming = self.request
        incoming.settimeout(3)
        try:
            record = b""
            while len(record) < 5:
                chunk = incoming.recv(5 - len(record))
                if not chunk:
                    return
                record += chunk
            length = int.from_bytes(record[3:5], "big")
            if length > 16384:
                raise ValueError("oversize TLS record")
            while len(record) < length + 5:
                chunk = incoming.recv(length + 5 - len(record))
                if not chunk:
                    return
                record += chunk
            protocols = alpns(record)
            allowed = bool(protocols) and all(p in ("h2", "http/1.1") for p in protocols)
            log(kind="dpi", alpn=protocols, allowed=allowed)
            if not allowed:
                incoming.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
                return
            with socket.create_connection(("93.184.216.34", 443), timeout=3) as upstream:
                upstream.sendall(record)
                sockets = [incoming, upstream]
                while True:
                    ready, _, _ = select.select(sockets, [], [], 5)
                    if not ready:
                        return
                    for source in ready:
                        chunk = source.recv(16384)
                        if not chunk:
                            return
                        (upstream if source is incoming else incoming).sendall(chunk)
        except (OSError, ValueError, IndexError, struct.error) as error:
            log(kind="dpi_error", error=str(error))


class Interceptor(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(3)
        try:
            with self.server.context.wrap_socket(self.request, server_side=True) as client:
                log(kind="intercept_accepted", alpn=client.selected_alpn_protocol())
                context = ssl.create_default_context(cafile=self.server.ca)
                context.set_alpn_protocols(["latch-relay/1"])
                with socket.create_connection(("93.184.216.34", 443), timeout=3) as raw:
                    with context.wrap_socket(raw, server_hostname="relay.netlab.test") as upstream:
                        sockets = [client, upstream]
                        while True:
                            ready, _, _ = select.select(sockets, [], [], 5)
                            if not ready:
                                return
                            for source in ready:
                                chunk = source.recv(16384)
                                if not chunk:
                                    return
                                (upstream if source is client else client).sendall(chunk)
        except (OSError, ssl.SSLError) as error:
            log(kind="intercept_rejected", error=str(error))


class Portal(http.server.BaseHTTPRequestHandler):
    def setup(self):
        super().setup()
        self.connection.settimeout(3)
        log(kind="portal_connection", peer=self.client_address)

    def do_GET(self):
        self.send_response(302)
        self.send_header("Location", "http://login.netlab.test/sign-in")
        self.end_headers()
        log(kind="portal", path=self.path)

    def log_message(self, *_):
        pass


class DNS64(socketserver.BaseRequestHandler):
    def handle(self):
        query, server = self.request
        offset = 12
        while query[offset]:
            offset += query[offset] + 1
        offset += 1
        kind = int.from_bytes(query[offset:offset + 2], "big")
        question = query[12:offset + 4]
        record_type = 28 if self.server.address_family == socket.AF_INET6 else 1
        address = (ipaddress.IPv6Address(int(ipaddress.IPv6Network(self.server.prefix).network_address) +
                                       int(ipaddress.IPv4Address("93.184.216.34"))).packed
                   if record_type == 28 else socket.inet_aton("93.184.216.34"))
        answer = b"\xc0\x0c" + struct.pack("!HHIH", record_type, 1, 30, len(address)) + address if kind == record_type else b""
        response = query[:2] + struct.pack("!HHHHH", 0x8180, 1, bool(answer), 0, 0)
        server.sendto(response + question + answer, self.client_address)
        log(kind="dns", qtype=kind, answer=socket.inet_ntop(self.server.address_family, address) if answer else None)


class TCPServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


class UDP6Server(socketserver.ThreadingUDPServer):
    address_family = socket.AF_INET6
    allow_reuse_address = True
    daemon_threads = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["dpi", "portal", "dns64", "dns", "intercept"])
    parser.add_argument("--cert")
    parser.add_argument("--key")
    parser.add_argument("--ca")
    parser.add_argument("--prefix", default="64:ff9b::/96")
    args = parser.parse_args()
    if args.mode == "dpi":
        server = TCPServer(("0.0.0.0", 8443), DPI)
    elif args.mode == "intercept":
        server = TCPServer(("0.0.0.0", 8443), Interceptor)
        server.context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        server.context.load_cert_chain(args.cert, args.key)
        server.context.set_alpn_protocols(["latch-relay/1", "h2", "http/1.1"])
        server.ca = args.ca
    elif args.mode == "portal":
        server = http.server.ThreadingHTTPServer(("0.0.0.0", 8080), Portal)
    elif args.mode == "dns":
        server = socketserver.ThreadingUDPServer(("0.0.0.0", 53), DNS64)
    else:
        server = UDP6Server(("::", 53), DNS64)
        server.prefix = args.prefix
    log(kind="ready", mode=args.mode)
    server.serve_forever()


if __name__ == "__main__":
    main()
