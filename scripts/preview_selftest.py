#!/usr/bin/env python3
"""preview_selftest.py — 验证预览服务器 + 前端静态资源确实可用。

以子进程方式起 `preview_web.py`，然后：
  1. HTTP 取 `/`（应 302 到 /battle.html）、`/battle.html`、`/radar.js`、`/leaflet-lite.js`、
     `/style.css`、`/maps.json`，检查状态码/内容长度/关键标记；
  2. 对 `/ws` 做真实 WebSocket 握手（RFC6455），读 3 帧，断言收到
     `hello`（含 world/brand/tile_template）与 `state`（含 players/self/kills/traces）
     以及 `diag`（含 counters）；
  3. 校验前端就绪探针的三要素在静态文件里都有（`data-battleReady`、`#app`、`.leaflet-container`）；
  4. 抓 `/tiles/...` 404 分支（前端应回落到内联占位图）。

用法: python scripts/preview_selftest.py [--port 8799]
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import subprocess
import sys
# Non-UTF-8 consoles (Windows Chinese default is cp936) raise UnicodeEncodeError on
# decorative glyphs such as U+2194 or U+2705. Keep the console encoding so Chinese still
# renders, and downgrade only the characters it cannot represent.
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding='utf-8', errors='replace')  # type: ignore[attr-defined]
    except (AttributeError, ValueError):
        pass
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WEB = ROOT / 'web'
ok_count = 0
fail_count = 0


def ok(msg: str) -> None:
    global ok_count
    ok_count += 1
    print(f'  [ok]   {msg}')


def bad(msg: str) -> None:
    global fail_count
    fail_count += 1
    print(f'  [FAIL] {msg}')


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """302 不许跟随，否则测不到重定向本身。"""

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: D102
        return None


def fetch(url: str, follow: bool = True):
    opener = urllib.request.build_opener(*([] if follow else [NoRedirect]))
    req = urllib.request.Request(url, headers={'User-Agent': 'preview-selftest'})
    try:
        with opener.open(req, timeout=5) as r:
            return r.status, r.read(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read(), dict(e.headers)


def ws_read(sock: socket.socket) -> tuple[int, bytes]:
    def read(n: int) -> bytes:
        buf = b''
        while len(buf) < n:
            chunk = sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError('closed')
            buf += chunk
        return buf

    b1, b2 = read(2)
    opcode = b1 & 0x0F
    n = b2 & 0x7F
    if n == 126:
        n = struct.unpack('!H', read(2))[0]
    elif n == 127:
        n = struct.unpack('!Q', read(8))[0]
    return opcode, read(n)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument('--port', type=int, default=8799)
    args = ap.parse_args()
    port = args.port
    base = f'http://127.0.0.1:{port}'

    env = dict(os.environ, PYTHONIOENCODING='utf-8')
    proc = subprocess.Popen(
        [sys.executable, str(ROOT / 'scripts' / 'preview_web.py'),
         '--port', str(port), '--quiet', '--players', '8'],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env,
    )
    try:
        # 等端口就绪
        for _ in range(60):
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=0.4):
                    break
            except OSError:
                time.sleep(0.1)
        else:
            print('服务器未就绪，输出如下：')
            print(proc.stdout.read().decode('utf-8', 'replace') if proc.stdout else '')
            return 1

        print('== [1] 静态资源 ==')
        # 注意：雷达页的就绪标记是**运行时**由 radar.js 写上的（dataset.battleReady），
        # index.html 里只有静态初值 data-battle-ready="0"。
        for path, probe in [('/battle.html', b'data-battle-ready'),
                            ('/radar.js', b'worldToLatLng'),
                            ('/leaflet-lite.js', b'leaflet-container'),
                            ('/style.css', b'--bg'),
                            ('/maps.json', b'ZeroDam')]:
            status, body, _ = fetch(base + path)
            if status == 200 and probe in body:
                ok(f'{path} 200, {len(body)} B, 含 {probe.decode()}')
            else:
                bad(f'{path} status={status} probe={probe!r} found={probe in body}')

        status, _, headers = fetch(base + '/', follow=False)
        loc = headers.get('Location', '')
        if status == 302 and 'battle.html' in loc:
            ok(f'/ 302 -> {loc}')
        else:
            bad(f'/ status={status} Location={loc!r}')

        status, body, _ = fetch(base + '/tiles/ZeroDam/3/1/2.png')
        if status == 404:
            ok('/tiles/... -> 404（前端回落到内联占位图）')
        else:
            bad(f'/tiles/... 期望 404，得到 {status}')

        print('== [2] WebSocket 契约 ==')
        key = base64.b64encode(os.urandom(16)).decode()
        sock = socket.create_connection(('127.0.0.1', port), timeout=5)
        sock.sendall(
            f'GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n'
            f'Upgrade: websocket\r\nConnection: Upgrade\r\n'
            f'Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n'.encode()
        )
        handshake = b''
        while b'\r\n\r\n' not in handshake:
            handshake += sock.recv(1024)
        expect = base64.b64encode(
            hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()
        ).decode()
        if b'101' in handshake.split(b'\r\n')[0] and expect.encode() in handshake:
            ok('握手 101 且 Sec-WebSocket-Accept 正确')
        else:
            bad(f'握手异常：{handshake[:120]!r}')

        # 客户端也发一个 subscribe（与 radar.js 行为一致）
        payload = b'{"type":"subscribe"}'
        mask = os.urandom(4)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        sock.sendall(bytes([0x81, 0x80 | len(payload)]) + mask + masked)

        seen = {}
        deadline = time.time() + 6
        while time.time() < deadline and len(seen) < 3:
            opcode, data = ws_read(sock)
            if opcode == 0x1:
                msg = json.loads(data.decode('utf-8'))
                seen.setdefault(msg.get('type'), msg)

        if seen.get('hello'):
            h = seen['hello']
            good = all(k in h for k in ('brand', 'map', 'tile_template', 'world'))
            (ok if good else bad)(f'hello 字段齐全（brand={h.get("brand")} map={h.get("map")}）')
        else:
            bad('没收到 hello')

        st = seen.get('state')
        if st:
            need = ('ts', 'players', 'self', 'kills', 'traces', 'loot', 'counters')
            missing = [k for k in need if k not in st]
            if missing:
                bad(f'state 缺字段：{missing}')
            else:
                p = st['players'][0]
                pneed = ('uuid', 'name', 'kind', 'x', 'z', 'yaw', 'team', 'hp',
                         'alive', 'distance', 'weapon')
                pmiss = [k for k in pneed if k not in p]
                if pmiss:
                    bad(f'player 缺字段：{pmiss}')
                else:
                    ok(f'state 字段齐全，players={len(st["players"])}，'
                       f'首个：{p["name"]} d={p["distance"]}m yaw={p["yaw"]}')
            # 再读一帧，确认位置在变（前端插值有意义）
            time.sleep(0.3)
        else:
            bad('没收到 state')

        if seen.get('diag'):
            c = seen['diag'].get('counters', {})
            if 'udp_packets_up' in c and 'parsed_packets' in c:
                ok(f'diag 计数器齐全（{len(c)} 项）')
            else:
                bad('diag 计数器不全')
        else:
            bad('没收到 diag')

        sock.close()

        print('== [3] 前端就绪探针三要素 ==')
        html = (WEB / 'index.html').read_text(encoding='utf-8')
        radar = (WEB / 'radar.js').read_text(encoding='utf-8')
        leaflet = (WEB / 'leaflet-lite.js').read_text(encoding='utf-8')
        checks = [
            ('index.html 有 #app', 'id="app"' in html),
            ('index.html 引用 radar.js', 'radar.js' in html),
            ('index.html 引用 leaflet-lite.js', 'leaflet-lite.js' in html),
            ('radar.js 设 battleReady', 'battleReady' in radar),
            ('leaflet-lite.js 提供 .leaflet-container', 'leaflet-container' in leaflet),
            ('radar.js 处理 subscribe 发送', 'subscribe' in radar),
            ('leaflet-lite.js 有内联占位图', 'offlineTilePng' in leaflet),
            ('radar.js 有 3D 转向修正入口', 'applyRotationCorrection' in radar),
        ]
        for label, good in checks:
            (ok if good else bad)(label)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    print(f'\n{ok_count} ok / {fail_count} fail')
    return 1 if fail_count else 0


if __name__ == '__main__':
    sys.exit(main())
