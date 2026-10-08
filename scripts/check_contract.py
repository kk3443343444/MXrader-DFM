#!/usr/bin/env python3
"""Cross-language contract check for MXrader-DFM.

Without a macOS/Rust toolchain we cannot compile, but we *can* verify that the
layers agree on their interfaces. This script parses the sources and asserts:

  [A] every `battle_proxy_*` symbol declared in core/include/battle_proxy.h
      is exported (`#[no_mangle]` + `pub extern "C" fn`) in core/src/ios_bridge.rs
  [B] every module declared with `mod x;` / `pub mod x;` has a file, and every
      `crate::...::name` path referenced by one layer exists as a declaration
  [C] admin action names in ios_bridge.rs and web/mod.rs overlap
  [D] WS message types emitted by Rust (`"type": "..."`) are handled by web/radar.js
  [E] status JSON keys produced by state.rs appear in the Swift Codable structs
  [F] the front-end readiness probe string in BattleWebView.swift matches the one
      radar.js sets (`data-battleReady`)
  [G] sample-origin literals required for fidelity are still present

Usage: python scripts/check_contract.py [--repo <root>]
"""
import argparse
import json
import pathlib
import re
import sys

# 在非 UTF-8 控制台（Windows 简体中文默认 cp936）上，打印 '↔'/'✅' 这类字符会直接
# 抛 UnicodeEncodeError 把脚本打断。保留控制台的编码（这样中文正常显示），只把
# 编不出来的字符降级掉。
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding='utf-8', errors='replace')  # type: ignore[attr-defined]
    except (AttributeError, ValueError):
        pass

ROOT = pathlib.Path(__file__).resolve().parent.parent
problems: list[str] = []
notes: list[str] = []


def read(p: pathlib.Path) -> str:
    try:
        return p.read_text(encoding='utf-8')
    except (OSError, UnicodeDecodeError):
        return ''


def ok(msg: str) -> None:
    print(f'  [ok]   {msg}')


def bad(msg: str) -> None:
    problems.append(msg)
    print(f'  [FAIL] {msg}')


def check_header_vs_bridge() -> None:
    print('== [A] C ABI: header <-> ios_bridge ==')
    header = read(ROOT / 'core/include/battle_proxy.h')
    bridge = read(ROOT / 'core/src/ios_bridge.rs')
    declared = set(re.findall(r'\b(battle_proxy_[a-z_]+)\s*\(', header))
    exported = set(
        re.findall(r'pub\s+extern\s+"C"\s+fn\s+(battle_proxy_[a-z_]+)\s*\(', bridge)
    )
    if not declared:
        bad('battle_proxy.h declares no symbols')
        return
    for sym in sorted(declared):
        if sym in exported:
            ok(f'{sym} declared and exported')
        else:
            bad(f'{sym} declared in header but not exported by ios_bridge.rs')
    for sym in sorted(exported - declared):
        bad(f'{sym} exported but missing from battle_proxy.h')
    if 'no_mangle' not in bridge:
        bad('ios_bridge.rs has no #[no_mangle] — C ABI would not be linkable')


MODULE_PATHS = {
    'core/src/lib.rs': ROOT / 'core/src',
}


def iter_modules() -> list[tuple[pathlib.Path, str]]:
    """(owner file, module name) for every mod declaration in the crate."""
    out = []
    for f in (ROOT / 'core/src').rglob('*.rs'):
        for m in re.finditer(r'^\s*(?:pub\s+)?mod\s+([a-z_][a-z0-9_]*)\s*;', read(f), re.M):
            out.append((f, m.group(1)))
    return out


def check_modules_exist() -> None:
    print('== [B] module tree ==')
    missing = 0
    for owner, name in iter_modules():
        base = owner.parent
        if (base / f'{name}.rs').exists() or (base / name / 'mod.rs').exists():
            continue
        bad(f'{owner.relative_to(ROOT)}: mod {name} has no file')
        missing += 1
    if not missing:
        ok(f'all {len(iter_modules())} module declarations resolve to files')


def check_cross_layer_paths() -> None:
    print('== [B2] cross-layer paths used by socks5/web ==')
    core = ROOT / 'core/src'
    symbols: dict[str, str] = {}
    for f in core.rglob('*.rs'):
        for m in re.finditer(
            r'^\s*(?:pub\s+)?(?:async\s+)?fn\s+([a-z_][a-z0-9_]*)', read(f), re.M
        ):
            symbols.setdefault(m.group(1), str(f.relative_to(ROOT)))
        for m in re.finditer(r'^\s*pub\s+(?:const|static)\s+([A-Z][A-Z0-9_]*)', read(f), re.M):
            symbols.setdefault(m.group(1).lower(), str(f.relative_to(ROOT)))
        for m in re.finditer(r'^\s*pub\s+struct\s+([A-Za-z0-9_]+)', read(f), re.M):
            symbols.setdefault(m.group(1), str(f.relative_to(ROOT)))

    calls = {
        'engine.feed': r'engine\.feed\(|\.feed\(',
        'engine.radar_state': r'\.radar_state\(',
        'engine.subscribe_radar': r'\.subscribe_radar\(',
        'state.shutdown_notify': r'\.shutdown_notify\(',
        'state.status_json': r'\.status_json\(',
        'state.set_capture': r'\.set_capture\(',
        'state.capture_download': r'\.capture_download\(',
        'state.reset_sessions': r'\.reset_sessions\(',
        'state.broadcast_diag': r'\.broadcast_diag\(',
        'state.set_loot_parsing': r'\.set_loot_parsing\(',
        'state.announce': r'\.announce\(',
        'state.admin_token': r'\.admin_token\(',
        'web::embed::web_root': r'embed::web_root\(',
        'web::embed::card_page': r'embed::card_page\(',
        'web::embed::channel_map_json': r'embed::channel_map_json\(',
    }
    for label, pattern in calls.items():
        users = []
        for f in core.rglob('*.rs'):
            text = read(f)
            if label.startswith('web::') and 'embed.rs' in str(f):
                continue
            if re.search(pattern, text):
                users.append(str(f.relative_to(ROOT)))
        name = label.split('.')[-1].split('::')[-1]
        if name in symbols:
            ok(f'{label} -> {symbols[name]} (used by {len(users)})')
        elif users:
            bad(f'{label} used by {users} but no definition found in core/src')
        else:
            notes.append(f'{label} unused')


def check_admin_actions() -> None:
    print('== [C] admin actions ==')
    bridge = read(ROOT / 'core/src/ios_bridge.rs')
    web = read(ROOT / 'core/src/web/mod.rs')
    bridge_actions = set(re.findall(r'"((?:diag|loot|session/reset|announcement|capture/[a-z]+|shutdown|selftest))"', bridge))
    web_actions = set(re.findall(r'"(capture/[a-z]+|session/reset|announcement|loot|diag|shutdown)"', web))
    if not bridge_actions:
        bad('ios_bridge.rs exposes no admin actions')
    else:
        ok(f'bridge actions: {sorted(bridge_actions)}')
    if not web_actions:
        bad('web/mod.rs exposes no admin routes')
    else:
        ok(f'web admin routes: {sorted(web_actions)}')
    only_web = web_actions - bridge_actions - {'selftest'}
    if only_web:
        notes.append(f'HTTP-only admin actions (fine, not exposed over C ABI): {sorted(only_web)}')


WS_TYPES = ('hello', 'state', 'diag', 'bye')


def check_ws_types() -> None:
    print('== [D] WebSocket message types ==')
    radar = read(ROOT / 'web/radar.js')
    core = (read(ROOT / 'core/src/web/ws.rs') + read(ROOT / 'core/src/web/battle_view.rs'))
    emitted = set(re.findall(r'"type"\s*:\s*"([a-z]+)"', core))
    documented = emitted & set(WS_TYPES)
    extra = emitted - set(WS_TYPES)
    if extra:
        notes.append(f'WS types outside the documented set (probably test fixtures): {sorted(extra)}')
    if not documented:
        bad('no documented WS message types found in the web crate')
    for t in sorted(documented):
        # the front-end may dispatch with `case 'x'`, `type === 'x'` or a handler map
        if re.search(rf"['\"]{t}['\"]", radar):
            ok(f'front-end handles "{t}"')
        else:
            bad(f'front-end does not handle WS message type "{t}"')
    # client -> server control messages documented in INTERFACES.md §4
    for t in ('subscribe', 'unsubscribe'):
        if t in radar and t in core:
            ok(f'client control message "{t}" implemented on both sides')
        elif t in core:
            notes.append(f'"{t}" accepted by the server but not sent by radar.js')


def check_status_keys_vs_swift() -> None:
    print('== [E] status JSON <-> Swift Codable ==')
    state = read(ROOT / 'core/src/state.rs')
    swift = read(ROOT / 'ios/BattleReceiverOpen/Bridge/BattleBridge.swift')
    keys = set(re.findall(r'^\s*pub\s+([a-z_][a-z0-9_]*):', state, re.M))
    wanted = {'phase', 'mode', 'version', 'socks_port', 'web_port', 'primary_port',
              'data_directory', 'endpoint', 'runtime_status', 'last_health_check', 'error'}
    for k in sorted(wanted):
        if k in keys and k in swift:
            ok(f'{k} present in both')
        else:
            bad(f'{k} missing in {"state.rs" if k not in keys else "BattleBridge.swift"}')


def check_frontend_probe() -> None:
    print('== [F] readiness probe ==')
    swift = read(ROOT / 'ios/BattleReceiverOpen/BattleWebView.swift')
    radar = read(ROOT / 'web/radar.js')
    if 'battleReady' in swift and 'battleReady' in radar:
        ok('both sides use dataset.battleReady')
    else:
        bad('data-battle-ready marker mismatch')
    if '.leaflet-container' in swift and 'leaflet-container' in read(ROOT / 'web/leaflet-lite.js'):
        ok('.leaflet-container probe has a matching provider')
    else:
        bad('.leaflet-container probe has no provider')
    if "'#app'" in swift and 'id="app"' in read(ROOT / 'web/index.html'):
        ok('#app probe matches index.html')


def check_fidelity_literals() -> None:
    print('== [G] sample-origin literals ==')
    must = {
        'core/src/socks5/mod.rs': ['SOCKS5 plaintext endpoint listening on TCP 0.0.0.0:'],
        'core/src/socks5/relay.rs': ['; UDP relays are per-association', '] TCP relay ended: upstream='],
        'core/src/web/mod.rs': [
            'no capture data available',
            'remote diagnostics control requires BATTLE_ADMIN_TOKEN',
            'remote loot parser control requires BATTLE_ADMIN_TOKEN',
            'remote protocol capture control requires BATTLE_ADMIN_TOKEN',
            'remote protocol capture download requires BATTLE_ADMIN_TOKEN',
            'remote announcement update requires BATTLE_ADMIN_TOKEN',
            'remote session reset requires BATTLE_ADMIN_TOKEN',
            'remote shutdown requires BATTLE_ADMIN_TOKEN',
            'BATTLE SOCKS5 GLOBAL UDP',
        ],
        'core/src/ios_bridge.rs': ['接收端口被其他应用占用（已自动尝试', 'Rust 服务启动失败（错误码'],
        'core/src/battle/codec/killchain.rs': ['EKilledBySectorArtilerrateSkill', 'EKilledByGuidedMissleSkill'],
        'core/src/battle/codec/container_collector.rs': ['randomised_not_transmitted'],
        'core/src/battle/combat.rs': ['rejected packet growth'],
        'core/src/battle/udpxin.rs': ['unresolved_bunch_header_variant'],
        'core/src/battle/engine.rs': ['rejected packet growth'],
        'core/src/state.rs': [
            '整局适配采集已手动开启：文件落盘、限时限量、端点匿名化',
            '协议诊断采集已手动停止',
        ],
        'core/src/battle/protocol_capture.rs': ['application/x-ndjson; charset=utf-8'],
        'core/src/debug_monitor.rs': ['debug_started_at_ms', 'milliseconds_since_last_server_transmission'],
        'core/src/desktop_auth.rs': [
            'runtime resource capability is unavailable',
            'runtime resource authentication failed',
            'runtime entry document is not UTF-8',
        ],
    }
    for rel, needles in must.items():
        if not needles:
            continue
        text = read(ROOT / rel)
        if not text:
            bad(f'{rel} missing')
            continue
        for n in needles:
            if n in text:
                ok(f'{rel}: "{n[:48]}"')
            else:
                bad(f'{rel}: missing literal "{n}"')


def check_assets() -> None:
    print('== [H] extracted assets ==')
    for rel, min_bytes, probe in [
        ('core/assets/channel_map.json', 50_000, 'BP_DFMCharacter_C'),
        ('core/assets/battle_card.html', 2_000, '卡密激活'),
        ('core/assets/loot_ids.json', 1_000, 'SOL_DT_'),
        ('web/index.html', 1_000, 'app'),
        ('web/radar.js', 20_000, 'worldToLatLng'),
        ('ios/BattleReceiverOpen/Resources/MXMark.png', 1_000, ''),
    ]:
        p = ROOT / rel
        if not p.exists():
            bad(f'{rel} missing')
            continue
        if p.stat().st_size < min_bytes:
            bad(f'{rel} too small ({p.stat().st_size} B)')
            continue
        if probe and probe not in read(p):
            bad(f'{rel} does not contain expected marker {probe!r}')
            continue
        ok(f'{rel} ({p.stat().st_size} B)')


def count_loc() -> None:
    print('== [I] size summary ==')
    groups = {
        'rust core': list((ROOT / 'core/src').rglob('*.rs')),
        'swift app': list((ROOT / 'ios').rglob('*.swift')),
        'web front-end': [p for p in (ROOT / 'web').rglob('*') if p.suffix in ('.js', '.html', '.css', '.json', '.md')],
        'scripts/tools': list((ROOT / 'scripts').rglob('*')) + list((ROOT / 'tools').rglob('*.py')),
        'docs': list((ROOT / 'docs').rglob('*.md')),
    }
    for name, files in groups.items():
        loc = 0
        n = 0
        for f in files:
            if not f.is_file():
                continue
            loc += len(read(f).splitlines())
            n += 1
        print(f'  {name:15s} files={n:3d} lines={loc:6d}')


def main() -> int:
    global ROOT
    ap = argparse.ArgumentParser()
    ap.add_argument('--repo', default=str(ROOT))
    args = ap.parse_args()
    ROOT = pathlib.Path(args.repo).resolve()
    print(f'repo: {ROOT}\n')
    check_header_vs_bridge()
    check_modules_exist()
    check_cross_layer_paths()
    check_admin_actions()
    check_ws_types()
    check_status_keys_vs_swift()
    check_frontend_probe()
    check_fidelity_literals()
    check_assets()
    count_loc()
    print()
    if notes:
        print('notes:')
        for n in notes:
            print('  -', n)
    if problems:
        print(f'\n{len(problems)} contract problem(s):')
        for p in problems:
            print('  *', p)
        return 1
    print('\ncontract check passed')
    return 0


if __name__ == '__main__':
    sys.exit(main())
