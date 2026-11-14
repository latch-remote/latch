#!/usr/bin/env python3
"""Run a supplied PowerShell script through the existing passwordless SSH alias."""
import base64
import pathlib
import subprocess
import sys

script = pathlib.Path(sys.argv[1]).read_text() if sys.argv[1] != '-c' else sys.argv[2]
encoded = base64.b64encode(script.encode('utf-16le')).decode('ascii')
raise SystemExit(subprocess.call(['ssh', '-oBatchMode=yes', 'windows',
                                 'powershell.exe', '-NoProfile', '-EncodedCommand', encoded]))
