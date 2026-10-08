#!/usr/bin/env python3
"""端到端验证：SOCKS5 TCP CONNECT + UDP ASSOCIATE 两条路都走一遍。

这是**游戏真正使用的路径**（UDP ASSOCIATE），所以比单测更有意义：
  1. 起一个本地 UDP echo 服务当"游戏服务器"
  2. 走 SOCKS5 握手 + UDP ASSOCIATE，检查 BND.ADDR 是否是可达地址（不能是 0.0.0.0）
  3. 按 SOCKS5 UDP 报文格式封装（RSV/FRAG/ATYP/DST/PORT + payload）发出去
  4. 校验回程报文：header 里的来源地址 + payload 是否一致
  5. 顺带用 TCP CONNECT 请求一次 HTTP，确认 TCP 中继也在工作
"""
from __future__ import annotations

import socket
import struct
import sys
import threading
import time

for _s in (sys.stdout, sys.stderr):
    try:
        _s.reconfigure(encoding='utf-8', errors='replace')
    except (AttributeError, ValueError):
        pass

PROXY_HOST = '127.0.0.1'
PROXY_PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 24000
ECHO_PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 24001

ok = True


def report(name: str, passed: bool, detail: str = '') -> None:
    global ok
    ok = ok and passed
    print(f'  [{"PASS" if passed else "FAIL"}] {name}' + (f'  {detail}' if detail else ''))


def echo_server(port: int) -> None:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(('127.0.0.1', port))
    while True:
        data, peer = s.recvfrom(65535)
        s.sendto(b'ECHO:' + data, peer)


def socks_connect_and_associate():
    """完成握手，返回 (tcp_socket, bnd_addr)。"""
    s = socket.create_connection((PROXY_HOST, PROXY_PORT), timeout=5)
    s.sendall(b'\x05\x01\x00')                      # VER=5, NMETHODS=1, no-auth
    reply = s.recv(2)
    assert reply == b'\x05\x00', f'握手失败: {reply!r}'

    s.sendall(b'\x05\x03\x00\x01' + socket.inet_aton('0.0.0.0') + struct.pack('!H', 0))
    head = s.recv(4)
    assert head[1] == 0x00, f'UDP ASSOCIATE 被拒: rep={head[1]}'
    atyp = head[3]
    if atyp == 0x01:
        addr = socket.inet_ntoa(s.recv(4))
    elif atyp == 0x04:
        addr = socket.inet_ntop(socket.AF_INET6, s.recv(16))
    else:
        n = s.recv(1)[0]
        addr = s.recv(n).decode()
    port = struct.unpack('!H', s.recv(2))[0]
    return s, (addr, port)


def main() -> int:
    print(f'=== SOCKS5 端到端验证  proxy={PROXY_HOST}:{PROXY_PORT}  echo=:{ECHO_PORT} ===')
    threading.Thread(target=echo_server, args=(ECHO_PORT,), daemon=True).start()
    time.sleep(0.3)

    # ---------- TCP CONNECT ----------
    # 目标用 web 端口（24001 之类）：那是真实的 TCP 服务，能返回 HTTP 响应，
    # 所以这条用例能同时验证「CONNECT 握手」和「中继真的把数据搬过去了」。
    print('\n[1] TCP CONNECT 中继')
    web_port = PROXY_PORT + 1
    try:
        c = socket.create_connection((PROXY_HOST, PROXY_PORT), timeout=5)
        c.sendall(b'\x05\x01\x00')
        assert c.recv(2) == b'\x05\x00'
        c.sendall(b'\x05\x01\x00\x01' + socket.inet_aton('127.0.0.1') + struct.pack('!H', web_port))
        rep = c.recv(10)
        report('CONNECT 请求被接受', rep[1] == 0x00, f'rep={rep[1]}')
        c.sendall(f'GET /battle.html HTTP/1.1\r\nHost: 127.0.0.1:{web_port}\r\nConnection: close\r\n\r\n'.encode())
        c.settimeout(5)
        data = b''
        while True:
            try:
                chunk = c.recv(65535)
            except socket.timeout:
                break
            if not chunk:
                break
            data += chunk
        report('经中继拿到 HTTP 响应', data.startswith(b'HTTP/1.1 200'),
               (data.split(b'\r\n', 1)[0].decode('latin1') if data else '(空)'))
        report('响应里是雷达页', b'battle-ready' in data)
        c.close()
    except Exception as e:  # noqa: BLE001
        report('TCP CONNECT', False, str(e))

    # ---------- UDP ASSOCIATE ----------
    print('\n[2] UDP ASSOCIATE 中继（游戏走的就是这条）')
    try:
        ctrl, (bnd_host, bnd_port) = socks_connect_and_associate()
        report('BND.ADDR 不是通配地址', bnd_host not in ('0.0.0.0', '::'),
               f'BND={bnd_host}:{bnd_port}')
        report('BND.PORT 是有效端口', bnd_port != 0, f'port={bnd_port}')

        target = (PROXY_HOST, ECHO_PORT)
        payload = b'hello-radar'
        pkt = (b'\x00\x00\x00' + b'\x01' + socket.inet_aton(target[0])
               + struct.pack('!H', target[1]) + payload)
        u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        u.settimeout(3)
        u.sendto(pkt, (bnd_host if bnd_host not in ('0.0.0.0', '::') else PROXY_HOST, bnd_port))
        try:
            data, _ = u.recvfrom(65535)
            # 回程格式：RSV(2) FRAG(1) ATYP(1) SRC(4) PORT(2) + payload
            rsv_frag, atyp = data[:3], data[3]
            src_ip = socket.inet_ntoa(data[4:8])
            src_port = struct.unpack('!H', data[8:10])[0]
            body = data[10:]
            report('回程报文格式正确', rsv_frag == b'\x00\x00\x00' and atyp == 0x01,
                   f'rsv/frag={rsv_frag!r} atyp={atyp}')
            report('回程来源是 echo 服务', (src_ip, src_port) == target, f'{src_ip}:{src_port}')
            report('payload 完整往返', body == b'ECHO:' + payload, f'{body!r}')
        except socket.timeout:
            report('UDP 往返', False, '3 秒内没收到任何回程报文（中继没有把响应送回来）')
        u.close()
        ctrl.close()
    except Exception as e:  # noqa: BLE001
        report('UDP ASSOCIATE', False, str(e))

    print('\n结论:', '两条路都通 ✅' if ok else '有失败项 ❌（见上）')
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
