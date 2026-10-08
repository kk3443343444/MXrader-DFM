#!/usr/bin/env python3
"""Raw-binary string + blob scanner.

1. Extracts long runs of printable/UTF-8 bytes (>= minlen) anywhere in the file.
2. Reports the longest blobs (embedded assets such as HTML/CSS/JS often live here).
3. Dumps all short strings to a text file for grepping.
"""
import re
import sys
import collections

PATH = sys.argv[1]
OUTDIR = sys.argv[2]

data = open(PATH, 'rb').read()
print('file size:', len(data))

# long blobs of mostly-printable bytes (embedded text assets)
blob_re = re.compile(rb'[\x09\x0a\x0d\x20-\x7e\xc2-\xf4][\x09\x0a\x0d\x20-\x7e\x80-\xbf]{199,}')
blobs = []
for m in blob_re.finditer(data):
    blobs.append((m.start(), m.group(0)))
print('long blobs (>=200B):', len(blobs))

blobs.sort(key=lambda t: -len(t[1]))
with open(OUTDIR + '/blobs.txt', 'wb') as fh:
    for off, b in blobs:
        fh.write(b'===== offset 0x%x len %d =====\n' % (off, len(b)))
        fh.write(b)
        fh.write(b'\n')
for off, b in blobs[:25]:
    head = b[:110].decode('utf-8', 'replace').replace('\n', '\\n')
    print('  0x%08x %8d  %s' % (off, len(b), head))

# short strings
str_re = re.compile(rb'[\x20-\x7e]{5,}')
cnt = collections.Counter()
for m in str_re.finditer(data):
    cnt[m.group(0).decode('ascii', 'replace')] += 1
with open(OUTDIR + '/allstrings.txt', 'w', encoding='utf-8') as fh:
    for s, c in sorted(cnt.items()):
        fh.write(s + '\n')
print('unique short strings:', len(cnt))
