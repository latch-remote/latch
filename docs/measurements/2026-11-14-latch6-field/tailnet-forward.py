#!/usr/bin/env python3
"""Private TCP/UDP measurement forwarder for a loopback-bound synthetic host.

Avoids an unsigned host's macOS incoming application-firewall restriction.
Traffic is capped at 2 GB including both directions and stops after 30 minutes.
"""
import pathlib
import selectors
import socket
import threading
import time

BIND = ('100.111.100.57', 47964)
HOST = ('127.0.0.1', 47965)
started = time.monotonic()
listener = socket.socket()
listener.bind(BIND)
listener.listen(2)
viewer_udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
viewer_udp.bind(BIND)
host_udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
host_udp.bind(('127.0.0.1', 0))
selector = selectors.DefaultSelector()
selector.register(listener, selectors.EVENT_READ)
selector.register(viewer_udp, selectors.EVENT_READ)
selector.register(host_udp, selectors.EVENT_READ)
deny_udp = pathlib.Path(__file__).with_name('deny-udp')
viewer = None
transferred = 0

def copy(source, destination):
    try:
        with source, destination:
            while data := source.recv(65536):
                destination.sendall(data)
    except OSError:
        pass
    finally:
        try:
            destination.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

try:
    while time.monotonic() - started < 1800 and transferred < 2_000_000_000:
        for key, _ in selector.select(1):
            if key.fileobj is listener:
                client, peer = listener.accept()
                if peer[0] != '100.106.23.40':
                    client.close()
                    continue
                host = socket.create_connection(HOST, timeout=3)
                host.settimeout(None)
                threading.Thread(target=copy, args=(client, host), daemon=True).start()
                threading.Thread(target=copy, args=(host, client), daemon=True).start()
            elif key.fileobj is viewer_udp:
                data, peer = viewer_udp.recvfrom(65536)
                if peer[0] == '100.106.23.40' and not deny_udp.exists():
                    viewer = peer
                    host_udp.sendto(data, HOST)
                    transferred += len(data)
            else:
                data, _ = host_udp.recvfrom(65536)
                if viewer and not deny_udp.exists():
                    viewer_udp.sendto(data, viewer)
                    transferred += len(data)
finally:
    selector.close()
    listener.close()
    viewer_udp.close()
    host_udp.close()
    print(f'forwarder_udp_bytes={transferred} elapsed_s={time.monotonic()-started:.3f}')
