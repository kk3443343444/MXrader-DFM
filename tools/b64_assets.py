#!/usr/bin/env python3
"""Decode the large embedded base64 blob(s): sniff magic, inflate/zip, dump assets."""
import base64
import binascii
import io
import re
import sys
import zlib
import gzip
import os

path = sys.argv[1]
outdir = sys.argv[2]
data = open(path, 'rb').read()
os.makedirs(outdir, exist_ok=True)

B64 = re.compile(rb'[A-Za-z0-9+/]{200,}={0,2}')
seen = 0
for m in B64.finditer(data):
    run = m.group(0)
    # trim leading zeros -> not part of base64 alphabet handling; try decode as-is
    try:
        raw = base64.b64decode(run, validate=True)
    except (binascii.Error, ValueError):
        continue
    if len(raw) < 100:
        continue
    magic = raw[:4]
    tag = None
    body = None
    if magic[:2] == b'\x1f\x8b':
        tag = 'gzip'
        try:
            body = gzip.decompress(raw)
        except Exception:
            body = None
    elif magic[:2] == b'\x78\x01' or magic[:2] == b'\x78\x5e' or magic[:2] == b'\x78\x9c' or magic[:2] == b'\x78\xda':
        tag = 'zlib'
        try:
            body = zlib.decompress(raw)
        except Exception:
            body = None
    elif magic[:2] == b'PK':
        tag = 'zip'
        body = raw
    elif magic[:1] == b'{' or magic[:1] == b'[':
        tag = 'json'
        body = raw
    elif b'<!DOCTYPE' in raw[:200] or b'<html' in raw[:400]:
        tag = 'html'
        body = raw
    else:
        continue
    name = '%s/b64_0x%x_%s.bin' % (outdir, m.start(), tag)
    open(name, 'wb').write(body if body else raw)
    print('0x%08x len_b64=%d %-6s -> %s (%d bytes)' % (m.start(), len(run), tag, name, len(body or raw)))
    if tag == 'zip':
        import zipfile
        try:
            with zipfile.ZipFile(io.BytesIO(raw)) as zf:
                print('   zip entries:', zf.namelist()[:50])
                zf.extractall(name + '_unzip')
        except Exception as e:
            print('   zip error', e)
    seen += 1
    if seen > 40:
        break
print('candidates:', seen)
