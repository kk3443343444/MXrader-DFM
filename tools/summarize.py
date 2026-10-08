#!/usr/bin/env python3
"""Summarize a Mach-O dump JSON: Swift modules, ObjC classes, notable symbols."""
import json
import re
import sys
import collections

path = sys.argv[1] if len(sys.argv) > 1 else 'dump.json'
d = json.load(open(path, encoding='utf-8'))
syms = [s['name'] for s in d['symbols']]

SWIFT = '$s'
mods = collections.Counter()
for s in syms:
    if s.startswith('_' + SWIFT):
        body = s[3:]
        m = re.match(r'^([0-9]+)', body)
        if m:
            n = int(m.group(1))
            mods[body[len(m.group(1)):len(m.group(1)) + n]] += 1
        else:
            mods['<nonnominal>'] += 1

print('== swift modules ==')
for k, v in mods.most_common(30):
    print('  %-24s %d' % (k, v))

print('\n== objc classes ==')
pref = '_OBJC_CLASS_$_'
for x in sorted(set(s[len(pref):] for s in syms if s.startswith(pref))):
    print('  ', x)

print('\n== strings by section ==')
for k, v in d['strings'].items():
    print('  %-22s %d' % (k, len(v)))
