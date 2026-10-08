#!/usr/bin/env bash
# verify_structure.sh — 校验产物 IPA 是否与参考样本“同结构”
#
# 检查项（全部来自对样本 BattleReceiverOpen 的逆向结论）：
#   [1] Payload/<App>.app 顶层布局：主二进制 + Info.plist + PkgInfo + 图标 + 品牌图
#   [2] 无 Frameworks/ 目录（Rust 静态链接进主二进制）
#   [3] Info.plist 关键键：CFBundleIdentifier / BattleBrandVariant / NSBonjourServices /
#       NSAppTransportSecurity / MinimumOSVersion / CFBundleExecutable
#   [4] 主二进制：arm64 MH_EXECUTE、无 LC_ENCRYPTION_INFO（即未加密）、
#       链接 SwiftUI/Combine/WebKit/CoreImage/Security、存在 LC_CODE_SIGNATURE
#   [5] C ABI 符号（battle_proxy_*）出现在符号表里
#   [6] Rust 侧可达：__cstring 里能搜到 crate 名 battle_proxy 与关键中文串
#
# 用法: ./verify_structure.sh <path/to/App.ipa> [参考样本 .ipa]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
TOOLS="$ROOT/tools"
PY="${PYTHON:-python3}"

IPA="${1:?用法: verify_structure.sh <App.ipa> [reference.ipa]}"
REF="${2:-}"

command -v "$PY" >/dev/null || { echo "错误：需要 python3（或用 PYTHON=... 指定）。" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

unzip -qo "$IPA" -d "$WORK/new"
APP="$(find "$WORK/new/Payload" -maxdepth 1 -name '*.app' | head -1)"
[[ -n "$APP" ]] || { echo "失败：IPA 里没有 Payload/*.app" >&2; exit 1; }
NAME="$(basename "$APP" .app)"
BIN="$APP/$NAME"
[[ -f "$BIN" ]] || { echo "失败：找不到主二进制 $NAME" >&2; exit 1; }

fail=0
ok()   { echo "  [ok]   $1"; }
bad()  { echo "  [FAIL] $1"; fail=$((fail+1)); }
info() { echo "  [info] $1"; }

echo "== [1] 顶层布局 =="
for f in Info.plist PkgInfo "$NAME"; do
  [[ -e "$APP/$f" ]] && ok "$f" || bad "缺少 $f"
done
icon_count=$(find "$APP" -maxdepth 1 -name 'MXIcon-*.png' | wc -l | tr -d ' ')
[[ "$icon_count" -ge 6 ]] && ok "MXIcon-*.png ×$icon_count" || bad "图标不足（$icon_count）"
for f in MXMark.png BattleMark.png; do
  [[ -e "$APP/$f" ]] && ok "$f" || info "可选品牌图缺失：$f"
done

echo "== [2] 无 Frameworks/ （Rust 静态链接） =="
if [[ -d "$APP/Frameworks" ]]; then
  bad "存在 Frameworks/ 目录 —— 与样本结构不一致（样本是静态链接）"
else
  ok "无 Frameworks/ 目录"
fi
[[ -d "$APP/PlugIns" ]] && info "存在 PlugIns/（样本没有；确认是否故意）" || ok "无 PlugIns/"

echo "== [3] Info.plist 关键键 =="
"$PY" - "$APP/Info.plist" <<'PY'
import plistlib, sys
p = plistlib.load(open(sys.argv[1], 'rb'))
need = {
    'CFBundleIdentifier': 'com.mxrader.monstervision',
    'CFBundleExecutable': 'BattleReceiverOpen',
    'BattleBrandVariant': 'mx',
    'MinimumOSVersion': '16.0',
}
bad = 0
for k, want in need.items():
    got = p.get(k)
    if got == want:
        print(f'  [ok]   {k} = {got}')
    else:
        print(f'  [FAIL] {k} = {got!r}（期望 {want!r}）'); bad += 1
bonjour = p.get('NSBonjourServices') or []
if '_battleproxy._tcp' in bonjour and '_battleproxy._udp' in bonjour:
    print('  [ok]   NSBonjourServices 含 _battleproxy._tcp/_udp')
else:
    print(f'  [FAIL] NSBonjourServices = {bonjour!r}'); bad += 1
ats = p.get('NSAppTransportSecurity') or {}
if ats.get('NSAllowsArbitraryLoads') and ats.get('NSAllowsLocalNetworking'):
    print('  [ok]   NSAppTransportSecurity 允许局域网 HTTP')
else:
    print(f'  [FAIL] NSAppTransportSecurity = {ats!r}'); bad += 1
print(f'  [info] CFBundleShortVersionString = {p.get("CFBundleShortVersionString")}')
print(f'  [info] UIRequiredDeviceCapabilities = {p.get("UIRequiredDeviceCapabilities")}')
sys.exit(1 if bad else 0)
PY
[[ $? -eq 0 ]] || fail=$((fail+1))

echo "== [4] Mach-O 结构 =="
"$PY" "$TOOLS/macho_scan.py" "$BIN" > "$WORK/macho.json"
"$PY" - "$WORK/macho.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))[0]
h = d['header']
print(f"  [info] {h['cputype']} {h['filetype']} ncmds={h['ncmds']} flags={h['flags']}")
bad = 0
if 'arm64' not in h['cputype']:
    print('  [FAIL] 不是 arm64'); bad += 1
else:
    print('  [ok]   arm64')
if h['filetype'] != 'MH_EXECUTE':
    print(f"  [FAIL] filetype={h['filetype']}，期望 MH_EXECUTE"); bad += 1
else:
    print('  [ok]   MH_EXECUTE（主程序，不是 dylib）')
cmds = {c['cmd']: c for c in d['load_commands']}
enc = [c for c in d['load_commands'] if 'ENCRYPTION_INFO' in c['cmd']]
if enc and enc[0].get('cryptid'):
    print('  [FAIL] 二进制仍加密（cryptid≠0）—— 需要先解密再打包'); bad += 1
else:
    print('  [ok]   未加密（无 LC_ENCRYPTION_INFO / cryptid=0）')
if 'LC_CODE_SIGNATURE' in cmds:
    print('  [ok]   LC_CODE_SIGNATURE 存在')
else:
    print('  [FAIL] 缺少 LC_CODE_SIGNATURE'); bad += 1
fw = [c.get('name', '') for c in d['load_commands'] if c['cmd'] in ('LC_LOAD_DYLIB', 'LC_LOAD_WEAK_DYLIB') or 'DYLIB' in c['cmd']]
for want in ['SwiftUI', 'Combine', 'WebKit', 'CoreImage', 'Security']:
    if any(want in f for f in fw):
        print(f'  [ok]   链接 {want}')
    else:
        print(f'  [FAIL] 未链接 {want}'); bad += 1
sys.exit(1 if bad else 0)
PY
[[ $? -eq 0 ]] || fail=$((fail+1))

echo "== [5] C ABI 符号 =="
"$PY" "$TOOLS/macho_dump.py" "$BIN" "$WORK/dump.json" >/dev/null
"$PY" - "$WORK/dump.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
names = {s['name'] for s in d['symbols']}
need = ['battle_proxy_version', 'battle_proxy_start', 'battle_proxy_stop',
        'battle_proxy_status', 'battle_proxy_admin', 'battle_proxy_free_string']
bad = 0
for n in need:
    if any(n == x.lstrip('_') for x in names) or n in ' '.join(names):
        print(f'  [ok]   {n}')
    else:
        print(f'  [FAIL] 缺少符号 {n}'); bad += 1
sys.exit(1 if bad else 0)
PY
[[ $? -eq 0 ]] || fail=$((fail+1))

echo "== [6] Rust 侧可达性 =="
"$PY" - "$BIN" <<'PY'
import re, sys
data = open(sys.argv[1], 'rb').read()
probes = [b'battle_proxy', b'radar session retained',
          '端口已绑定，正在等待雷达页面'.encode(), b'SOCKS5 TCP/UDP']
bad = 0
for p in probes:
    if data.count(p) > 0:
        print(f'  [ok]   找到 {p!r} ×{data.count(p)}')
    else:
        print(f'  [FAIL] 未找到 {p!r}'); bad += 1
sys.exit(1 if bad else 0)
PY
[[ $? -eq 0 ]] || fail=$((fail+1))

if [[ -n "$REF" ]]; then
  echo "== [7] 与参考样本对比 =="
  unzip -qo "$REF" -d "$WORK/ref"
  "$PY" - "$WORK/ref" "$WORK/new" <<'PY'
import os, sys
def layout(root):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in ('Payload',)]
        for f in filenames:
            yield os.path.relpath(os.path.join(dirpath, f), root)
ref = {p.split(os.sep, 1)[1] if os.sep in p else p for p in layout(sys.argv[1])}
new = {p.split(os.sep, 1)[1] if os.sep in p else p for p in layout(sys.argv[2])}
missing = sorted(x for x in ref - new if not x.startswith('_') and 'META-INF' not in x)
extra = sorted(x for x in new - ref if 'META-INF' not in x)
print('  仅在样本中：', missing or '（无）')
print('  仅在本地产物中：', extra or '（无）')
PY
fi

echo
if [[ $fail -eq 0 ]]; then
  echo "结构校验通过 ✅"
else
  echo "结构校验发现 $fail 类问题 ❌" >&2
  exit 1
fi
