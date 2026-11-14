#!/usr/bin/env python3
"""Freeze Git tracked and nonignored files, reading each file once for tar and hash."""
import hashlib
import io
import json
import pathlib
import subprocess
import tarfile
import time

root = pathlib.Path(__file__).resolve().parents[3]
report = pathlib.Path(__file__).resolve().parent
paths = sorted(set(subprocess.check_output(
    ['git', 'ls-files', '-co', '--exclude-standard', '-z'], cwd=root).split(b'\0')) - {b''})
archive = pathlib.Path('/tmp/latch6-field-source.tar.gz')
manifest = []
with tarfile.open(archive, 'w:gz') as tar:
    for raw in paths:
        name = raw.decode()
        if name.startswith('docs/measurements/2026-11-14-latch6-field/'):
            continue
        path = root / name
        if not path.is_file():
            continue
        data = path.read_bytes()
        entry = tarfile.TarInfo(name)
        entry.size = len(data)
        entry.mtime = int(time.time())
        entry.mode = path.stat().st_mode & 0o777
        tar.addfile(entry, io.BytesIO(data))
        manifest.append({'path': name, 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()})
payload = json.dumps(manifest, indent=2) + '\n'
(report / 'source-manifest.json').write_text(payload)
identity = {'utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
            'git_head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip(),
            'branch': subprocess.check_output(['git', 'branch', '--show-current'], cwd=root, text=True).strip(),
            'manifest_sha256': hashlib.sha256(payload.encode()).hexdigest(),
            'archive_sha256': hashlib.sha256(archive.read_bytes()).hexdigest(),
            'files': len(manifest), 'bytes': sum(row['bytes'] for row in manifest)}
(report / 'source-identity.json').write_text(json.dumps(identity, indent=2) + '\n')
print(json.dumps(identity))
