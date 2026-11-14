#!/usr/bin/env python3
"""Measure synthetic input/audio with only the scratch Windows host's UDP denied."""
import os
import pathlib
import subprocess

root = pathlib.Path(__file__).resolve().parent
windows = ['python3', str(root / 'windows.py'), '-c']
enable = r'''$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue'; $binary="$env:USERPROFILE\dev\latch6-field-20261114\target\debug\examples\serve.exe"; foreach ($direction in 'Inbound','Outbound') { New-NetFirewallRule -Name "Latch6FieldReverseUDP$direction" -DisplayName 'Latch6 reverse synthetic UDP denial' -Direction $direction -Action Block -Protocol UDP -LocalPort 47968 -RemoteAddress 100.111.100.57 -Program $binary -Profile Any | Out-Null }'''
disable = r'''$ProgressPreference='SilentlyContinue'; foreach ($direction in 'Inbound','Outbound') { Remove-NetFirewallRule -Name "Latch6FieldReverseUDP$direction" -ErrorAction SilentlyContinue }'''
try:
    subprocess.run(windows + [enable], check=True)
    environment = os.environ.copy()
    environment['RUST_LOG'] = 'latch_stream=info'
    binary = root.parents[2] / 'target/debug/latch-field-headless'
    with (root / 'final-reverse-relay-input.json').open('w') as output, (root / 'final-reverse-relay-input.err').open('w') as errors:
        subprocess.run([str(binary), '100.106.23.40', '--port', '47968', '--width', '1280', '--height', '720',
                        '--fps', '60', '--kbps', '10000', '--seconds', '15', '--keys', '20', '--audio', '--mute'],
                       stdout=output, stderr=errors, env=environment, timeout=100, check=True)
finally:
    subprocess.run(windows + [disable], check=False)
