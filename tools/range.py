#!/usr/bin/env python3
"""Dump a byte range of the binary as UTF-8 text (for reading embedded assets)."""
import sys

path, start, end = sys.argv[1], int(sys.argv[2], 0), int(sys.argv[3], 0)
data = open(path, 'rb').read()
seg = data[start:end]
sys.stdout.reconfigure(encoding='utf-8', errors='replace')
print(seg.decode('utf-8', 'replace'))
