#!/usr/bin/env python3
"""Sample the deployed relay cgroup and persistent quota without generating traffic."""
import argparse
import datetime
import json
from pathlib import Path
import subprocess
import time


def snapshot():
    cgroup = Path('/sys/fs/cgroup/system.slice/latch-relay.service')
    cpu = dict(line.split() for line in (cgroup / 'cpu.stat').read_text().splitlines())
    pid = subprocess.check_output(
        ['systemctl', 'show', 'latch-relay.service', '-p', 'MainPID', '--value'], text=True
    ).strip()
    status = Path('/proc', pid, 'status').read_text().splitlines()
    rss_kib = next(int(line.split()[1]) for line in status if line.startswith('VmRSS:'))
    ledger = Path('/var/lib/latch-relay/quota').read_bytes()
    assert len(ledger) == 32 and ledger[:8] == b'LATCHQ01', 'invalid quota ledger'
    return {
        'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'pid': int(pid), 'rss_kib': rss_kib,
        'cgroup_memory_bytes': int((cgroup / 'memory.current').read_text()),
        'cgroup_memory_peak_bytes': int((cgroup / 'memory.peak').read_text()),
        'cpu_usage_usec': int(cpu['usage_usec']),
        'quota_reserved_bytes': int.from_bytes(ledger[16:24], 'big'),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('seconds', type=int)
    parser.add_argument('--interval', type=int, default=30)
    args = parser.parse_args()
    if not 1 <= args.seconds <= 7200 or not 1 <= args.interval <= 60:
        parser.error('seconds must be 1..7200 and interval 1..60')
    started = time.monotonic()
    previous = snapshot()
    print(json.dumps(previous), flush=True)
    previous_time = started
    while time.monotonic() - started < args.seconds:
        time.sleep(min(args.interval, args.seconds - (time.monotonic() - started)))
        current = snapshot()
        now = time.monotonic()
        current['elapsed_s'] = now - started
        current['service_cpu_one_core_percent'] = (
            (current['cpu_usage_usec'] - previous['cpu_usage_usec']) / (now - previous_time) / 10000
        )
        print(json.dumps(current), flush=True)
        previous, previous_time = current, now


if __name__ == '__main__':
    main()
