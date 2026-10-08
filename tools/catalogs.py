#!/usr/bin/env python3
"""Pull the embedded UE class/channel catalog JSON and other structured blobs."""
import json
import re
import sys

path = sys.argv[1]
outdir = sys.argv[2]
data = open(path, 'rb').read()

hits = [m.start() for m in re.finditer(rb'\{"slot":', data)]
print('channel_map occurrences:', [hex(h) for h in hits])
for h in hits:
    # walk back to the opening brace of the outer object
    s = data.rfind(b'{', 0, h)
    # walk forward to the matching close brace
    depth = 0
    i = s
    while i < len(data):
        c = data[i]
        if c == 0x7b:
            depth += 1
        elif c == 0x7d:
            depth -= 1
            if depth == 0:
                break
        i += 1
    blob = data[s:i + 1]
    print('json blob at 0x%x len=%d' % (s, len(blob)))
    try:
        obj = json.loads(blob.decode('utf-8'))
        name = '%s/channel_map_0x%x.json' % (outdir, s)
        json.dump(obj, open(name, 'w', encoding='utf-8'), ensure_ascii=False, indent=1)
        print('  keys:', list(obj)[:20])
        if 'channel_map' in obj:
            cm = obj['channel_map']
            print('  channel_map entries:', len(cm))
            classes = sorted({e.get('class', '') for e in cm})
            print('  classes:', classes[:60])
        # look for sibling catalogs
        for k in obj:
            if isinstance(obj[k], dict):
                print('  catalog %s: %d entries' % (k, len(obj[k])))
                sample = list(obj[k].items())[:5]
                print('    sample:', sample)
    except Exception as e:
        print('  parse failed:', e)
        print('  head:', blob[:300])
