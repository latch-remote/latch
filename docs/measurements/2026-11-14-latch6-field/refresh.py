#!/usr/bin/env python3
"""Archive explicitly named source updates and maintain an effective hash manifest."""
import hashlib
import io
import json
import pathlib
import sys
import tarfile
import time

report = pathlib.Path(__file__).resolve().parent
root = report.parents[2]
label, *names = sys.argv[1:]
assert names, 'supply the exact source paths authorized for refresh'
rows = {row['path']: row for row in json.loads((report / 'effective-source-manifest.json').read_text())}
updates = []
archive = pathlib.Path('/tmp') / f'latch6-{label}.tar.gz'
with tarfile.open(archive, 'w:gz') as tar:
    for name in names:
        assert name in rows, f'path absent from the frozen source manifest: {name}'
        data = (root / name).read_bytes()
        row = {'path': name, 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
        rows[name] = row
        updates.append(row)
        entry = tarfile.TarInfo(name)
        entry.size = len(data)
        entry.mtime = int(time.time())
        tar.addfile(entry, io.BytesIO(data))
identity = {'utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), 'files': updates,
            'archive_sha256': hashlib.sha256(archive.read_bytes()).hexdigest()}
(report / f'{label}-refresh.json').write_text(json.dumps(identity, indent=2)+'\n')
manifest = json.dumps(sorted(rows.values(), key=lambda row: row['path']), indent=2)+'\n'
(report / 'effective-source-manifest.json').write_text(manifest)
(report / 'effective-source-identity.json').write_text(json.dumps({
    'base_manifest_sha256': json.loads((report / 'source-identity.json').read_text())['manifest_sha256'],
    'effective_manifest_sha256': hashlib.sha256(manifest.encode()).hexdigest(),
    'files': len(rows)}, indent=2)+'\n')
print(archive)
print(json.dumps(identity))
