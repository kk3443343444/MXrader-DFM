#!/usr/bin/env python3
"""preview_web.py — 在 PC 上直接预览雷达前端（不需要设备、不需要 Rust 工具链）。

它做两件事：

1. 按 `web/mod.rs` 的路由把 `web/` 目录当静态站点提供
   （`/` → `/battle.html` → `web/index.html`，另加 `/maps.json`、JS/CSS、`/tiles/...` 404）；
2. 在 `/ws` 上实现一个**假的核心**，按 docs/INTERFACES.md §5 推送
   `hello` / `state` / `diag` 消息，里面是合成的一局对局：自己 + 队友 + 敌人 + 人机 +
   死亡盒 + 物资箱 + 弹道 + 击杀条，全部在动。

用途：
* 立刻确认前端能不能起来（`data-battle-ready`、`.leaflet-container`、`#app` 三条探针）；
* 看 2D北向上 / 跟随朝向 / 3D 三种视角与转向修正在真实渲染下长什么样；
* 当成 WS 契约的可执行文档 —— Rust 侧实际发的字段以本文件 + INTERFACES.md §5 为准。

用法：
    python scripts/preview_web.py                      # http://127.0.0.1:8770/battle.html
    python scripts/preview_web.py --port 9000 --players 16 --map Layali
    python scripts/preview_web.py --list-maps
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import math
import os
import random
import re
import socket
import struct
import sys
# Non-UTF-8 consoles (Windows Chinese default is cp936) raise UnicodeEncodeError on
# decorative glyphs such as U+2194 or U+2705. Keep the console encoding so Chinese still
# renders, and downgrade only the characters it cannot represent.
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding='utf-8', errors='replace')  # type: ignore[attr-defined]
    except (AttributeError, ValueError):
        pass
import threading
import time
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WEB = ROOT / 'web'
GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'

MIME = {
    '.html': 'text/html; charset=utf-8',
    '.js': 'application/javascript; charset=utf-8',
    '.css': 'text/css; charset=utf-8',
    '.json': 'application/json; charset=utf-8',
    '.png': 'image/png',
    '.jpg': 'image/jpeg',
    '.webp': 'image/webp',
    '.svg': 'image/svg+xml',
    '.md': 'text/markdown; charset=utf-8',
}

KILL_TYPES = [
    'EKilledByWeapon', 'EkilledBySelf', 'EkilledByPoisonGas', 'EKilledFallDown',
    'EKilledFromImpendingDeath', 'EKilledFromBuff', 'EKilledFromEnvExplosion',
    'EKilledByVehicleWeapon', 'EKilledByAssassinateDamage',
    'EKilledByBattleFieldSupportSkill', 'EKilledBySectorArtilerrateSkill',
    'EKilledByGuidedMissleSkill',
]
WEAPONS = ['M4A1', 'AKM', 'SCAR-L', 'MP5', 'UZI', 'M870', 'SVD', 'M249', 'AWM', 'QBZ95']
NAMES = ['老六', '三号突击手', '摸金校尉', '医疗兵-7', '狙击手K', '冲锋号', '黑鹰', '静默',
         '夜枭', '拾荒者', '狙击手J', '报点王', 'Engineer-2', 'CQB', '四号位', '白给王']


def load_maps() -> dict:
    try:
        raw = json.loads((WEB / 'maps.json').read_text(encoding='utf-8'))
    except Exception:
        return {}
    return {k: v for k, v in raw.items() if not k.startswith('_') and isinstance(v, dict)}


class Scenario:
    """合成对局：位置随时间演化，保证前端的插值/排序/裁剪都能被看到。"""

    def __init__(self, players: int, map_key: str, seed: int = 7):
        self.rnd = random.Random(seed)
        self.map_key = map_key
        self.t0 = time.time()
        self.players = []
        self.kills = []
        self.camp = 0
        span = 900.0  # 世界范围（米），够铺满一张地图
        for i in range(players):
            kind = 'player'
            if i == 0:
                kind = 'self'
            elif i in (1, 2):
                kind = 'teammate'
            elif i >= max(3, players - 3):
                kind = 'ai'
            team = 0 if kind in ('self', 'teammate') else 1
            self.players.append({
                'uuid': f'G{i + 1:016X}',
                'name': '我' if kind == 'self' else self.rnd.choice(NAMES) + (f'#{i}' if i > 3 else ''),
                'kind': kind,
                'team': team,
                'camp': self.rnd.randint(0, 2),
                'hp': 100.0,
                'max_hp': 100.0,
                'alive': True,
                'weapon': self.rnd.choice(WEAPONS),
                'hero_id': self.rnd.choice([1001, 1002, 1003, 1024]),
                'level': self.rnd.randint(1, 60),
                'rank_score': self.rnd.randint(800, 4200),
                'yaw': self.rnd.uniform(0, 360),
                'pitch': self.rnd.uniform(-25, 25),
                'phase': self.rnd.uniform(0, math.tau),
                'radius': self.rnd.uniform(30, span * 0.45),
                'speed': self.rnd.uniform(0.03, 0.16),
                'x': self.rnd.uniform(-span / 2, span / 2),
                'z': self.rnd.uniform(-span / 2, span / 2),
            })
        # 本地玩家固定在原点，其它人围绕它运动 —— 这样 distance 才有意义。
        self.players[0]['x'] = 0.0
        self.players[0]['z'] = 0.0
        self.players[0]['radius'] = 0.0
        # 死亡盒与物资箱
        self.boxes = [{
            'channel': 900 + i, 'class': 'DFMContainerDataCollector', 'kind': 'loot',
            'x': self.rnd.uniform(-span / 2, span / 2), 'y': self.rnd.uniform(-span / 2, span / 2),
            'z': 0.0, 'contents_status': 'randomised_not_transmitted',
            'value': 0, 'items': [], 'last_seen_ms': 0,
        } for i in range(10)]

    def hello(self, brand: str) -> dict:
        return {
            'type': 'hello',
            'brand': brand,
            'map': self.map_key,
            'tile_template': f'/tiles/{self.map_key}/{{z}}/{{x}}/{{y}}.png',
            'read_only_radar': False,
            'world': {'origin_x': 0.0, 'origin_y': 0.0, 'scale': 1e-5,
                      'yaw_offset_deg': 0.0, 'yaw_sign': 1.0},
        }

    def state(self, tick: int) -> dict:
        now = int(time.time() * 1000)
        t = time.time() - self.t0
        self_p = self.players[0]
        players = []
        for p in self.players:
            ang = p['phase'] + t * p['speed']
            x = math.cos(ang) * p['radius']
            z = math.sin(ang * 1.3) * p['radius'] * 0.7
            if p['kind'] == 'self':
                x, z = 0.0, 0.0
            yaw = (math.degrees(math.atan2(math.sin(ang) * p['radius'] * 1.3,
                                           -math.sin(ang) * p['radius'])) + 90.0) % 360.0
            dx, dz = x - self_p['x'], z - self_p['z']
            dist = math.hypot(dx, dz)
            players.append({
                'uuid': p['uuid'],
                'name': p['name'],
                'kind': 'ai' if p['kind'] == 'ai' else 'player',
                'x': round(x, 2), 'y': 0.0, 'z': round(z, 2),
                'vx': 0.0, 'vy': 0.0, 'vz': 0.0,
                'yaw': round(yaw, 2), 'pitch': round(p['pitch'], 2), 'roll': 0.0,
                'team': p['team'], 'camp': p['camp'],
                'hp': p['hp'], 'max_hp': p['max_hp'],
                'alive': p['alive'], 'visible': True,
                'distance': round(dist, 1),
                'weapon': p['weapon'], 'hero_id': p['hero_id'],
                'level': p['level'], 'rank_score': p['rank_score'],
                'is_ai': p['kind'] == 'ai',
                'last_seen_ms': now, 'source': 'move',
                'self': p['kind'] == 'self',
            })
        # 每 ~2 秒造一条击杀、每 ~4 秒造一条弹道
        if tick % 40 == 0:
            killer = self.rnd.choice(players)
            victim = self.rnd.choice([p for p in players if p['uuid'] != killer['uuid']])
            self.kills.insert(0, {
                'ts': now,
                'killer': killer['name'], 'victim': victim['name'],
                'weapon': self.rnd.choice(WEAPONS),
                'damage_type': KILL_TYPES[self.rnd.randrange(len(KILL_TYPES))],
                'damage': round(self.rnd.uniform(20, 140), 1),
                'distance': round(self.rnd.uniform(5, 420), 1),
                'x': victim['x'], 'y': victim['z'], 'z': 0.0,
                'revenge': self.rnd.random() < 0.2, 'assist': False,
                'local': killer.get('self') or victim.get('self'),
            })
            del self.kills[20:]
        traces = []
        if tick % 80 == 0:
            src = self.rnd.choice(players)
            ln = self.rnd.uniform(30, 250)
            rad = math.radians(src['yaw'])
            traces.append({
                'x1': src['x'], 'y1': src['z'], 'z1': 1.6,
                'x2': src['x'] + math.sin(rad) * ln,
                'y2': src['z'] + math.cos(rad) * ln,
                'z2': 1.6,
                'shooter_uuid': src['uuid'], 'weapon_class': src['weapon'],
                'ts_ms': now, 'confidence': 'ballistic_trajectory_complete',
            })
        for b in self.boxes:
            b['last_seen_ms'] = now
        return {
            'type': 'state', 'ts': now, 'tick': tick,
            'map': self.map_key,
            'self': next((p for p in players if p.get('self')), None),
            'players': players, 'kills': self.kills, 'loot': self.boxes, 'traces': traces,
            'counters': {
                'active_sessions': 1, 'total_sessions': 1,
                'udp_packets_up': tick * 37, 'udp_packets_down': tick * 41,
                'udp_invalid_packets': 3, 'udp_outbound_sockets': 2,
                'tcp_relay_failures': 0, 'udp_relay_bytes': tick * 5120,
                'loot_payloads_skipped': 0, 'parse_queue_depth': 0,
                'parsed_packets': tick * 78, 'matched_entities': len(players),
            },
            'sessions': 1, 'uptime_ms': int(t * 1000),
        }

    def diag(self, tick: int) -> dict:
        return {
            'type': 'diag', 'ts': int(time.time() * 1000), 'tick': tick,
            'counters': {
                'active_sessions': 1, 'total_sessions': 1,
                'udp_packets_up': tick * 37, 'udp_packets_down': tick * 41,
                'udp_invalid_packets': 3, 'udp_outbound_sockets': 2,
                'tcp_relay_failures': 0, 'udp_relay_bytes': tick * 5120,
                'loot_payloads_skipped': 0, 'parse_queue_depth': 0,
                'parsed_packets': tick * 78, 'matched_entities': 0,
            },
        }


# --------------------------------------------------------------------------
# 极简 WebSocket（服务端）
# --------------------------------------------------------------------------

def ws_accept(key: str) -> str:
    return base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()


def ws_send_text(sock: socket.socket, text: str) -> None:
    payload = text.encode('utf-8')
    header = bytearray([0x81])
    n = len(payload)
    if n < 126:
        header.append(n)
    elif n < 65536:
        header.append(126)
        header += struct.pack('!H', n)
    else:
        header.append(127)
        header += struct.pack('!Q', n)
    sock.sendall(bytes(header) + payload)


def ws_recv(sock: socket.socket) -> tuple[int, bytes] | None:
    def read(n: int) -> bytes:
        buf = b''
        while len(buf) < n:
            chunk = sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError('closed')
            buf += chunk
        return buf

    try:
        b1, b2 = read(2)
    except ConnectionError:
        return None
    opcode = b1 & 0x0F
    masked = bool(b2 & 0x80)
    n = b2 & 0x7F
    if n == 126:
        n = struct.unpack('!H', read(2))[0]
    elif n == 127:
        n = struct.unpack('!Q', read(8))[0]
    mask = read(4) if masked else b'\x00\x00\x00\x00'
    data = bytearray(read(n))
    if masked:
        for i in range(n):
            data[i] ^= mask[i % 4]
    return opcode, bytes(data)


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    scenario: Scenario
    brand = 'mx'
    quiet = False

    def log_message(self, fmt, *args):  # noqa: D102
        if not self.quiet:
            sys.stderr.write('  %s\n' % (fmt % args))

    # ---- 静态文件 ----
    def do_GET(self):  # noqa: N802
        path = self.path.split('?', 1)[0]
        if path in ('/', '/index.html'):
            return self.redirect('/battle.html?brand=' + self.brand)
        if path == '/ws':
            return self.upgrade_ws()
        if path in ('/battle.html', '/license'):
            return self.serve_file(WEB / 'index.html')
        if path.startswith('/tiles/'):
            self.log_message('tile miss %s -> 404 (front-end falls back to its placeholder)', path)
            self.send_error(HTTPStatus.NOT_FOUND, 'no tile set')
            return
        rel = path.lstrip('/')
        if not re.fullmatch(r'[A-Za-z0-9_./-]+', rel) or '..' in rel:
            return self.send_error(HTTPStatus.BAD_REQUEST, 'bad path')
        return self.serve_file(WEB / rel)

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get('Content-Length') or 0)
        body = self.rfile.read(length) if length else b''
        if self.path.split('?', 1)[0] in ('/license/activate',):
            payload = json.dumps({'ok': True, 'authorized': True, 'card_tail': '9F2C',
                                  'echo': body.decode('utf-8', 'replace')}).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json; charset=utf-8')
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        self.send_error(HTTPStatus.NOT_FOUND)

    def serve_file(self, path: Path) -> None:
        if not path.is_file():
            return self.send_error(HTTPStatus.NOT_FOUND, 'not found')
        data = path.read_bytes()
        self.send_response(200)
        self.send_header('Content-Type', MIME.get(path.suffix, 'application/octet-stream'))
        self.send_header('Content-Length', str(len(data)))
        self.send_header('Cache-Control', 'no-store')
        self.end_headers()
        self.wfile.write(data)

    def redirect(self, where: str) -> None:
        self.send_response(302)
        self.send_header('Location', where)
        self.send_header('Content-Length', '0')
        self.end_headers()

    # ---- WebSocket ----
    def upgrade_ws(self) -> None:
        key = self.headers.get('Sec-WebSocket-Key')
        if not key:
            return self.send_error(HTTPStatus.BAD_REQUEST, 'missing Sec-WebSocket-Key')
        self.send_response(101)
        self.send_header('Upgrade', 'websocket')
        self.send_header('Connection', 'Upgrade')
        self.send_header('Sec-WebSocket-Accept', ws_accept(key))
        self.end_headers()

        sock = self.connection
        sock.settimeout(None)
        try:
            ws_send_text(sock, json.dumps(self.scenario.hello(self.brand)))
            tick = 0
            last_diag = 0.0
            self.log_message('ws client attached (preview scenario)')
            while True:
                tick += 1
                ws_send_text(sock, json.dumps(self.scenario.state(tick), ensure_ascii=False))
                if time.time() - last_diag > 2.0:
                    last_diag = time.time()
                    ws_send_text(sock, json.dumps(self.scenario.diag(tick)))
                # 顺便读一下客户端控制帧（subscribe/ping/close）
                sock.settimeout(0.0)
                try:
                    got = ws_recv(sock)
                except (BlockingIOError, socket.timeout):
                    got = None
                except ConnectionError:
                    break
                finally:
                    sock.settimeout(0.05)
                if got:
                    opcode, data = got
                    if opcode == 0x8:
                        break
                    if opcode == 0x9:
                        sock.sendall(bytes([0x8A, len(data)]) + data)
                time.sleep(0.05)
        except (ConnectionError, OSError, BrokenPipeError):
            pass
        finally:
            try:
                sock.close()
            except OSError:
                pass


def main() -> int:
    ap = argparse.ArgumentParser(description='雷达前端预览服务器（假数据）')
    ap.add_argument('--host', default='127.0.0.1')
    ap.add_argument('--port', type=int, default=8770)
    ap.add_argument('--players', type=int, default=12)
    ap.add_argument('--map', default='ZeroDam')
    ap.add_argument('--brand', default='mx')
    ap.add_argument('--seed', type=int, default=7)
    ap.add_argument('--quiet', action='store_true')
    ap.add_argument('--open', action='store_true', help='尝试用系统默认浏览器打开')
    ap.add_argument('--list-maps', action='store_true')
    args = ap.parse_args()

    maps = load_maps()
    if args.list_maps:
        print('maps.json 里的地图键：')
        for k in maps:
            print('  ', k)
        return 0
    if args.map not in maps:
        print(f'提示：{args.map} 不在 maps.json 里（会用前端内置默认标定）。可选：{", ".join(maps)}')

    if not (WEB / 'index.html').is_file():
        print(f'找不到 {WEB / "index.html"}', file=sys.stderr)
        return 1

    Handler.scenario = Scenario(args.players, args.map, args.seed)
    Handler.brand = args.brand
    Handler.quiet = args.quiet

    httpd = ThreadingHTTPServer((args.host, args.port), Handler)
    url = f'http://{args.host}:{args.port}/battle.html?brand={args.brand}'
    print(f'雷达前端预览： {url}')
    print(f'  - 假数据：{args.players} 个玩家 + 物资箱 + 击杀条 + 弹道，25 Hz 推送')
    print(f'  - 地图键：{args.map}   视角：左侧图例里的 2D北向上/跟随朝向/3D')
    print('  - Ctrl+C 退出')
    if args.open:
        import webbrowser
        threading.Timer(0.5, lambda: webbrowser.open(url)).start()
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print('\nbye')
    return 0


if __name__ == '__main__':
    sys.exit(main())
