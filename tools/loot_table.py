#!/usr/bin/env python3
"""Extract the embedded compressed item-name table (GameItem id -> asset name)
from the reference binary and write a JSON sample for the replica's loot catalog."""
import json
import re
import sys
from collections import OrderedDict

path, out = sys.argv[1], sys.argv[2]
data = open(path, 'rb').read()

pair = re.compile(rb'"([0-9]{8,12})":"(SOL_DT_[A-Za-z0-9_\[\]#]{4,120})"')
items = OrderedDict()
for m in pair.finditer(data):
    key = m.group(1).decode()
    val = m.group(2).decode()
    if key not in items:
        items[key] = val

# also capture the bracketed dictionary entries (Term#... macros) so the
# expansion logic in loot_catalog.rs can be exercised with real data
terms = OrderedDict()
term_re = re.compile(rb'\[Term#([0-9]{6,16})_([A-Za-z]{3,32})\]_([0-9]{1,4})')
for m in term_re.finditer(data):
    k = 'Term#%s_%s' % (m.group(1).decode(), m.group(2).decode())
    terms.setdefault(k, m.group(2).decode())

print('unique item ids :', len(items))
print('unique terms    :', len(terms))
sample = OrderedDict(sorted(items.items())[:400])
json.dump(sample, open(out, 'w', encoding='utf-8'), ensure_ascii=False, indent=1)
print('wrote', out, 'entries:', len(sample))
if items:
    print('first:', next(iter(items.items())))
    print('last :', next(reversed(items.items())))
