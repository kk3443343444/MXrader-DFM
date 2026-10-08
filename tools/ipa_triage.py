#!/usr/bin/env python3
"""快速体检一批 IPA：能不能直接侧载、要不要卡密、是什么形态。

只做只读分析，输出一张表：
  * 包名 / 版本 / 最低系统 / 设备族
  * 主二进制架构、是否加密（cryptid）
  * 有没有 embedded.mobileprovision（有 = 带签名，能直接装；没有 = 要自己签）
  * 是否出现"卡密/激活/授权"相关字符串（决定要不要买卡）
  * 是否自带 web 雷达（Leaflet / battle.html / WS）
用法: python tools/ipa_triage.py <file.ipa> [file2.ipa ...]
"""
import plistlib
import re
import sys
import zipfile
from pathlib import Path

# 非 UTF-8 控制台（Windows 中文默认 cp936）打印某些字符会抛 UnicodeEncodeError，
# 保留控制台编码（中文正常显示），只降级编不出来的字符。
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding='utf-8', errors='replace')  # type: ignore[attr-defined]
    except (AttributeError, ValueError):
        pass

CARD_PAT = re.compile(
    r'(卡密|激活|授权|未激活|license|activation|card_key|vip|到期|会员)'.encode('utf-8'))
WEB_PAT = re.compile(rb'(leaflet-container|battle\.html|new WebSocket|radar\.js|/ws\b)')
PROVISION_PAT = b'embedded.mobileprovision'


def macho_arch(data: bytes) -> str:
    if len(data) < 8:
        return '?'
    magic = int.from_bytes(data[:4], 'little')
    if magic == 0xFEEDFACF:
        cpu = int.from_bytes(data[4:8], 'little')
        return {0x0100000C: 'arm64', 0x01000007: 'x86_64'}.get(cpu, hex(cpu)) + '/64'
    if magic == 0xFEEDFACE:
        return '32-bit'
    if data[:4] == b'PK\x03\x04':
        return 'zip(嵌套)'
    return '?'


def cryptid(data: bytes) -> int | None:
    """在 64 位 Mach-O 里找 LC_ENCRYPTION_INFO(_64)，返回 cryptid。"""
    if int.from_bytes(data[:4], 'little') != 0xFEEDFACF:
        return None
    import struct
    ncmds = struct.unpack_from('<I', data, 16)[0]
    off = 32
    for _ in range(ncmds):
        cmd, size = struct.unpack_from('<II', data, off)
        if cmd in (0x21, 0x2D):
            cid = struct.unpack_from('<I', data, off + 8)[0]
            return cid
        if size == 0:
            break
        off += size
    return 0


def triage(path: Path) -> dict:
    out = {'file': path.name, 'size_mb': round(path.stat().st_size / 1048576, 1)}
    try:
        zf = zipfile.ZipFile(path)
    except zipfile.BadZipFile:
        out['error'] = '不是 zip/ipa'
        return out

    names = zf.namelist()
    # 有些打包脚本不给目录项加尾斜杠，用更宽的条件找 .app
    apps = sorted({n.split('.app/')[0] + '.app/' for n in names if '.app/' in n
                   and n.startswith('Payload/')})
    if not apps:
        out['error'] = 'Payload 下没有 .app'
        out['top_entries'] = names[:8]
        return out
    app = apps[0]
    out['app'] = app.split('/')[1]

    # Info.plist
    try:
        info = plistlib.loads(zf.read(app + 'Info.plist'))
        out['bundle_id'] = info.get('CFBundleIdentifier')
        out['version'] = info.get('CFBundleShortVersionString')
        out['min_os'] = info.get('MinimumOSVersion')
        out['device_family'] = info.get('UIDeviceFamily')
        out['display_name'] = info.get('CFBundleDisplayName') or info.get('CFBundleName')
    except KeyError:
        out['error'] = '没有 Info.plist'
        return out

    # 是否有 provisioning / 签名
    out['has_provision'] = any(n.startswith(app + 'embedded.mobileprovision') or
                               n.endswith('embedded.mobileprovision') for n in names)
    out['frameworks'] = sum(1 for n in names if '/Frameworks/' in n and n.endswith('.dylib'))
    out['dylibs_tweak'] = [n for n in names if n.endswith('.dylib') and 'Frameworks' in n][:6]

    # 主二进制
    exe = info.get('CFBundleExecutable')
    if exe:
        try:
            data = zf.read(app + exe)
            out['main_bin_mb'] = round(len(data) / 1048576, 1)
            out['arch'] = macho_arch(data)
            out['cryptid'] = cryptid(data)
            out['has_card_strings'] = bool(CARD_PAT.search(data))
            out['has_web_radar'] = bool(WEB_PAT.search(data))
            m = CARD_PAT.findall(data)
            out['card_hits'] = sorted({s.decode('utf-8', 'replace') for s in m})[:8]
        except KeyError:
            out['error'] = f'主二进制 {exe} 读不到'

    # 其它资源里是否有 web 前端
    web_assets = [n for n in names if n.endswith(('.html', '.js')) and 'Frameworks' not in n]
    out['web_assets'] = web_assets[:6]
    return out


def main() -> int:
    paths = [Path(p) for p in sys.argv[1:]]
    if not paths:
        print(__doc__)
        return 2
    for p in paths:
        d = triage(p)
        print('=' * 78)
        print(f"{d['file']}  ({d.get('size_mb')} MB)")
        for k, v in d.items():
            if k in ('file', 'size_mb'):
                continue
            print(f'   {k:18} {v}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
