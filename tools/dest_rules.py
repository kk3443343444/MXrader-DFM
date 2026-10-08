#!/usr/bin/env python3
"""按采集文件生成"只转发游戏服务器"的分流规则。

背景：iOS 的第三方 VPN（小火箭 / Hiddify / sing-box）**做不到按 App 分流** ——
按 App 的 VPN 需要 MDM 或 App 自己声明 per-app VPN，侧载的代理 App 没有这个能力。
能做的只有"按目标地址分流"：只把游戏服务器的 IP 段/域名送进代理，其余直连。
于是需要一份准确的目标清单，而我们的协议采集正好记录了每条的 src/dst
（见 core/src/battle/codec/capture.rs 的 CaptureEntry）。

用法：
    py tools/dest_rules.py battle-full-capture-*.ndjson
    py tools/dest_rules.py capture.ndjson --rules        # 输出小火箭规则行
    py tools/dest_rules.py capture.ndjson --prefix 24    # 按 /24 聚合（默认）

小火箭里的用法：设置 → 全局路由选"配置"，把输出的 IP-CIDR 行粘到配置的 [Rule] 段，
**默认策略设成 DIRECT**，只让这些网段走代理节点。
"""
from __future__ import annotations

import collections
import ipaddress
import json
import pathlib
import sys

for _s in (sys.stdout, sys.stderr):
    try:
        _s.reconfigure(encoding='utf-8', errors='replace')
    except (AttributeError, ValueError):
        pass


def load(path: pathlib.Path):
    """读 ndjson；容忍半行（采集被强制结束时最后一行可能不完整）。"""
    out = []
    with path.open(encoding='utf-8', errors='replace') as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return out


def split_host(value: str) -> str:
    """`1.2.3.4:5678` / `[2001:db8::1]:80` / `2001:db8::1` -> 主机部分。"""
    v = value.strip()
    if v.startswith('['):
        return v[1:].split(']', 1)[0]
    if v.count(':') == 1:
        return v.split(':', 1)[0]
    return v


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith('--')]
    want_rules = '--rules' in sys.argv
    prefix = 24
    for a in sys.argv[1:]:
        if a.startswith('--prefix'):
            prefix = int(a.split('=', 1)[1]) if '=' in a else 24
    if not args:
        print(__doc__)
        return 2

    path = pathlib.Path(args[0])
    if not path.exists():
        print(f'找不到文件：{path}')
        return 1

    entries = load(path)
    dst_counter: collections.Counter[str] = collections.Counter()
    for e in entries:
        dst = split_host(str(e.get('dst', '')))
        if dst:
            dst_counter[dst] += 1

    if not dst_counter:
        print(f'读了 {len(entries)} 条，但没有任何 dst 字段 —— 采集里只有本机一侧？')
        return 1

    v4: collections.Counter[str] = collections.Counter()
    other: collections.Counter[str] = collections.Counter()
    for host, n in dst_counter.items():
        try:
            ip = ipaddress.ip_address(host)
        except ValueError:
            other[host] += n
            continue
        if isinstance(ip, ipaddress.IPv4Address):
            v4[str(ipaddress.ip_network(f'{host}/{prefix}', strict=False))] += n
        else:
            other[host] += n

    print(f'文件: {path.name}')
    print(f'记录: {len(entries)} 条   不同目标: {len(dst_counter)} 个\n')

    if want_rules:
        print('# 小火箭 / sing-box 分流规则（只让游戏服务器走代理，其余直连）')
        print('# 生成的依据：本机协议采集里的 dst 字段')
        print(f'# 聚合粒度：IPv4 /{prefix}')
        print('[Rule]')
        for net, n in v4.most_common():
            print(f'IP-CIDR,{net},PROXY,no-resolve   # {n} 条')
        for host, n in other.most_common(40):
            print(f'DOMAIN-SUFFIX,{host},PROXY   # {n} 条')
        print('FINAL,DIRECT')
        return 0

    print(f'--- IPv4 /{prefix} 段（按条数排序）---')
    for net, n in v4.most_common(25):
        print(f'  {net:20} {n:8} 条')
    if other:
        print('\n--- 非 IPv4 目标（域名/IPv6）---')
        for host, n in other.most_common(15):
            print(f'  {host:40} {n:6} 条')
    print('\n提示：加 --rules 输出可直接粘进小火箭的规则行。')
    return 0


if __name__ == '__main__':
    sys.exit(main())
