#!/usr/bin/env python3
"""Reconstruct the crate's module tree and feature surface from a raw string dump."""
import re
import sys
import collections

src = open(sys.argv[1], encoding='utf-8', errors='replace').read()

# 1) crate-local source paths
local = set(re.findall(r'(?<![\w/.])src/[A-Za-z0-9_/]*\.rs', src))
print('== battle_proxy crate files (%d) ==' % len(local))
for p in sorted(local):
    print('   ', p)

# 2) rust module paths
mods = set(re.findall(r'battle_proxy::[a-z0-9_:]+', src))
print('\n== rust modules (%d) ==' % len(mods))
for m in sorted(mods):
    print('   ', m)

# 3) bracketed log tags
tags = collections.Counter(re.findall(r'\[[a-z_]{2,24}\]', src))
print('\n== log tags ==')
for t, c in tags.most_common(80):
    print('   %-24s %d' % (t, c))

# 4) html / js / css markers
for pat in ['<!DOCTYPE', '<html', '<script', '<canvas', 'getContext', 'requestAnimationFrame',
            'WebSocket', 'canvas', '<div', 'style=', 'battle.html', 'innerHTML', 'function(',
            'const ', 'let ', '=>', 'data-battle']:
    n = src.count(pat)
    if n:
        print('asset marker %-22s %d' % (pat, n))
