#!/usr/bin/env python3
"""Extract the existing native viewer's metric lines without media or input data."""
import json
import pathlib
import re
import statistics

root = pathlib.Path(__file__).resolve().parent
metrics = ['fps', 'received_fps', 'mbps', 'received_mbps', 'rtt_ms', 'decode_ms',
           'decode_p95_ms', 'host_ms', 'assembly_ms', 'queue_ms', 'frames_lost',
           'frames_dropped', 'frame_jitter_ms', 'applied_bitrate_kbps', 'wire_budget_kbps']
summary = {}
for path in sorted(root.glob('*.err')):
    data = path.read_bytes()
    source = data.decode('utf-16' if data.startswith(b'\xff\xfe') else 'utf-8-sig', errors='replace')
    rows = []
    for line in source.splitlines():
        match = re.search(r'field elapsed_s=([\d.]+) relay=(true|false) decoded=(\d+) rendered=(\d+) stats ', line)
        if not match:
            continue
        row = {'elapsed_s': float(match[1]), 'relay': match[2] == 'true',
               'decoded': int(match[3]), 'rendered': int(match[4])}
        for metric in metrics:
            value = re.search(r'\b' + metric + r': ([-+\deE.]+)[, }]', line)
            if value:
                row[metric] = float(value[1])
        for metric in ['decoder', 'encoder', 'codec', 'audio']:
            value = re.search(r'\b' + metric + r': "([^"]*)"', line)
            if value:
                row[metric] = value[1]
        if rows:
            previous = rows[-1]
            seconds = row['elapsed_s'] - previous['elapsed_s']
            row['sample_duration_s'] = seconds
            row['render_submissions_fps'] = (row['rendered']-previous['rendered']) / seconds
            row['decode_count_fps'] = (row['decoded']-previous['decoded']) / seconds
        rows.append(row)
    if not rows:
        continue
    (root / (path.stem + '.metrics.json')).write_text(json.dumps(rows, indent=2)+'\n')
    capped = len(rows) > 1 and rows[-1]['rendered'] == rows[-2]['rendered'] == 16384
    if capped:
        for row in rows:
            if row['rendered'] >= 16384:
                row['render_counter_censored'] = True
                row.pop('render_submissions_fps', None)
        (root / (path.stem + '.metrics.json')).write_text(json.dumps(rows, indent=2)+'\n')
    result = {'samples': len(rows), 'last': rows[-1], 'phases': {}}
    if capped:
        result['render_counter_censored_after_s'] = next(row['elapsed_s'] for row in rows if row.get('render_counter_censored'))
    for relay in [False, True]:
        phase = [r for r in rows if r['elapsed_s'] >= 5 and r['relay'] == relay]
        if not phase:
            continue
        result['phases']['relay' if relay else 'udp'] = {
            'count_derived_decode_fps': sum(r.get('decode_count_fps',0)*r.get('sample_duration_s',0) for r in phase)/max(1e-9,sum(r.get('sample_duration_s',0) for r in phase)),
            'count_derived_render_submissions_fps': sum(r['render_submissions_fps']*r.get('sample_duration_s',0) for r in phase if 'render_submissions_fps' in r)/max(1e-9,sum(r.get('sample_duration_s',0) for r in phase if 'render_submissions_fps' in r)),
            'zero_decode_sample_seconds': sum(r.get('sample_duration_s',0) for r in phase if r.get('decode_count_fps',0)==0),
            'samples': len(phase), 'first_s': phase[0]['elapsed_s'], 'last_s': phase[-1]['elapsed_s'],
            'median': {key: statistics.median([r[key] for r in phase if key in r])
                       for key in metrics+['render_submissions_fps', 'decode_count_fps']},
            'max': {key: max(r.get(key, 0) for r in phase)
                    for key in ['rtt_ms', 'queue_ms', 'frames_lost', 'frames_dropped', 'frame_jitter_ms']}}
    result['render_summary'] = next((line for line in source.splitlines() if line.startswith('render_summary')), '')
    summary[path.stem] = result
(root / 'stream-summary.json').write_text(json.dumps(summary, indent=2)+'\n')
for name, result in summary.items():
    print(name, result['render_summary'])
    for phase, stats in result['phases'].items():
        print(phase, json.dumps(stats))
