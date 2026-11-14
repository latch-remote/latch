#!/usr/bin/env python3
"""Launch one native viewer and time only its private UDP/public TCP denial."""
import json
import pathlib
import subprocess
import time

root = pathlib.Path(__file__).resolve().parent
marker = root / 'deny-udp'
launch = r'''$ProgressPreference='SilentlyContinue'; $d="$env:USERPROFILE\dev\latch6-field-final-20261114"; $a=New-ScheduledTaskAction -Execute powershell.exe -Argument "-NoProfile -File $d\run-viewer.ps1 -Label final-outage2-noise-1080-60 -Fps 60 -Seconds 90 -Automatic -Audio" -WorkingDirectory $d; Set-ScheduledTask -TaskName Latch6Field20261114 -Action $a | Out-Null; Start-ScheduledTask -TaskName Latch6Field20261114'''
subprocess.run(['python3', str(root / 'windows.py'), '-c', launch], check=True)
start = time.monotonic()

def at(seconds):
    time.sleep(max(0, start + seconds - time.monotonic()))

def note(action):
    with (root / 'impairment-events.jsonl').open('a') as log:
        log.write(json.dumps({'utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
                              'run': 'final-outage2-noise-1080-60', 'action': action}) + '\n')

try:
    at(10)
    marker.touch()
    note('private UDP denied')
    at(22)
    marker.unlink(missing_ok=True)
    note('private UDP restored before public TCP denial')
    subprocess.run(['python3', str(root / 'windows.py'), str(root / 'deny-relay.ps1')], check=True)
    at(37)
    marker.touch()
    note('private UDP denied for fresh public attachment')
    at(65)
finally:
    marker.unlink(missing_ok=True)
    note('private UDP restored')
