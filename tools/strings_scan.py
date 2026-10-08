#!/usr/bin/env python3
"""Extract and classify every printable string in a Mach-O, per section.
Also pulls Swift reflection strings and SwiftUI accessibility tokens that
reveal the UI tree, and flags URLs / IPs / ports / protocol tokens."""
import json
import re
import sys
import collections

PATH = sys.argv[1]
OUT = sys.argv[2] if len(sys.argv) > 2 else None
d = json.load(open(PATH, encoding='utf-8'))

pats = {
    'url': re.compile(r'(https?|wss?|socks5|tcp|udp|rtsp)://[^\s"\']+'),
    'host_port': re.compile(r'\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}(?::[0-9]{1,5})?\b'),
    'ipv6': re.compile(r'\b(?:[0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{2,4}\b'),
    'path': re.compile(r'/[A-Za-z0-9_./-]{4,}'),
}

allstr = collections.Counter()
for sec, vals in d['strings'].items():
    for v in vals:
        allstr[v] += 1

print('== total unique strings:', len(allstr))
for kind, rx in pats.items():
    hits = sorted(set(m.group(0) for v in allstr for m in [rx.search(v)] if m))
    print('\n== %s (%d) ==' % (kind, len(hits)))
    for h in hits[:400]:
        print('  ', h)

kw = ['radar', 'Radar', 'proxy', 'Proxy', 'socks', 'SOCKS', 'player', 'Player', 'bone', 'Bone',
      'esp', 'ESP', 'aim', 'Aim', 'decrypt', 'Decrypt', 'proto', 'Proto', 'packet', 'Packet',
      'map', 'Map', 'matrix', 'Matrix', 'world', 'World', 'screen', 'Screen', 'yaw', 'pitch',
      'delta', 'Delta', 'battle', 'Battle', 'receiver', 'Receiver', 'tunnel', 'relay', 'Relay',
      'offset', 'Offset', 'weapon', 'Weapon', 'mesh', 'Mesh', 'unity', 'Unity', 'proto3',
      'protobuf', 'snappy', 'lz4', 'zlib', 'deflate', 'aes', 'AES', 'rc4', 'xor', 'blowfish',
      'chacha', 'md5', 'sha1', 'sha256', 'hmac', 'token', 'Token', 'license', 'License',
      '激活', '卡密', '授权', '雷达', '透视', '距离', '玩家', '方向', '坐标', '设置']
print('\n== keyword hits ==')
for k in kw:
    hits = [v for v in allstr if k in v and len(v) < 200]
    if hits:
        print('\n[%s] %d' % (k, len(hits)))
        for h in sorted(hits)[:60]:
            print('   ', h)

if OUT:
    with open(OUT, 'w', encoding='utf-8') as fh:
        for v, c in allstr.most_common():
            fh.write('%5d  %s\n' % (c, v))
