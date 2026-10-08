#!/usr/bin/env python3
"""Locate and extract the embedded web asset (HTML/JS/CSS) from a raw binary."""
import re
import sys
import zlib
import gzip

path = sys.argv[1]
outdir = sys.argv[2]
data = open(path, 'rb').read()

for marker in [b'<!DOCTYPE', b'<html', b'<script', b'battleReady', b'battle.html', b'data-battle']:
    idxs = [m.start() for m in re.finditer(re.escape(marker), data)]
    print('%-14s %d hits -> %s' % (marker.decode(), len(idxs), [hex(i) for i in idxs[:10]]))

# dump context around the first <!DOCTYPE
for m in re.finditer(rb'<!DOCTYPE', data):
    off = m.start()
    print('\n--- context at 0x%x ---' % off)
    print(data[max(0, off - 200):off + 600].decode('utf-8', 'replace'))

# look for deflate/gzip streams that decompress into HTML
print('\n--- scanning for zlib/gzip streams ---')
found = 0
for off in range(0, len(data) - 2):
    if data[off] == 0x78 and data[off + 1] in (0x01, 0x5e, 0x9c, 0xda):
        try:
            d = zlib.decompressobj()
            out = d.decompress(data[off:off + 4_000_000], 8_000_000)
        except Exception:
            continue
        if b'<html' in out[:200000] or b'<script' in out[:200000] or b'DOCTYPE' in out[:200000]:
            found += 1
            p = '%s/zlib_0x%x.html' % (outdir, off)
            open(p, 'wb').write(out)
            print('zlib stream at 0x%x -> %d bytes decompressed -> %s' % (off, len(out), p))
            os_path = p
            if found > 20:
                break
print('deflate streams with html:', found)
