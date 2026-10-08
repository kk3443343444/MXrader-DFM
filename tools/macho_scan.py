#!/usr/bin/env python3
"""Mach-O structural scanner: header, load commands, dylibs, rpaths,
encryption info, entitlements, and a load-command diff-ready dump."""
import struct
import sys
import plistlib

CPU_TYPES = {7: 'x86', 0x01000007: 'x86_64', 12: 'arm', 0x0100000C: 'arm64', 0x0200000C: 'arm64_32'}
MH_FLAGS = {
    0x1: 'NOUNDEFS', 0x2: 'INCRLINK', 0x4: 'DYLDLINK', 0x8: 'BINDATLOAD',
    0x10: 'PREBOUND', 0x20: 'SPLIT_SEGS', 0x40: 'LAZY_INIT', 0x80: 'TWOLEVEL',
    0x100: 'FORCE_FLAT', 0x200: 'NOMULTIDEFS', 0x400: 'NOFIXPREBINDING',
    0x800: 'PREBINDABLE', 0x1000: 'ALLMODSBOUND', 0x2000: 'SUBSECTIONS_VIA_SYMBOLS',
    0x4000: 'CANONICAL', 0x8000: 'WEAK_DEFINES', 0x10000: 'BINDS_TO_WEAK',
    0x20000: 'ALLOW_STACK_EXECUTION', 0x40000: 'ROOT_SAFE', 0x80000: 'SETUID_SAFE',
    0x100000: 'NO_REEXPORTED_DYLIBS', 0x200000: 'PIE', 0x400000: 'DEAD_STRIPPABLE_DYLIB',
    0x800000: 'HAS_TLV_DESCRIPTORS', 0x1000000: 'NO_HEAP_EXECUTION',
    0x2000000: 'APP_EXTENSION_SAFE', 0x4000000: 'NLIST_OUTOFSYNC_WITH_DYLDINFO',
    0x8000000: 'SIM_SUPPORT', 0x10000000: 'DYLIB_IN_CACHE',
}
PLATFORMS = {1: 'macOS', 2: 'iOS', 3: 'tvOS', 4: 'watchOS', 5: 'bridgeOS', 6: 'macCatalyst',
             7: 'iOSSimulator', 8: 'tvOSSimulator', 9: 'watchOSSimulator', 10: 'driverKit',
             11: 'visionOS', 12: 'visionOSSimulator'}
LC = {
    0x1: 'LC_SEGMENT', 0x2: 'LC_SYMTAB', 0x3: 'LC_SYMSEG', 0x4: 'LC_THREAD', 0x5: 'LC_UNIXTHREAD',
    0x6: 'LC_LOADFVMLIB', 0x7: 'LC_IDFVMLIB', 0x8: 'LC_IDENT', 0x9: 'LC_FVMFILE',
    0xa: 'LC_PREPAGE', 0xb: 'LC_DYSYMTAB', 0xc: 'LC_LOAD_DYLIB', 0xd: 'LC_ID_DYLIB',
    0xe: 'LC_LOAD_DYLINKER', 0xf: 'LC_ID_DYLINKER', 0x10: 'LC_PREBOUND_DYLIB',
    0x11: 'LC_ROUTINES', 0x12: 'LC_SUB_FRAMEWORK', 0x13: 'LC_SUB_UMBRELLA',
    0x14: 'LC_SUB_CLIENT', 0x15: 'LC_SUB_LIBRARY', 0x16: 'LC_TWOLEVEL_HINTS',
    0x17: 'LC_PREBIND_CKSUM', 0x18: 'LC_LOAD_WEAK_DYLIB', 0x19: 'LC_SEGMENT_64',
    0x1a: 'LC_ROUTINES_64', 0x1b: 'LC_UUID', 0x1c: 'LC_RPATH', 0x1d: 'LC_CODE_SIGNATURE',
    0x1e: 'LC_SEGMENT_SPLIT_INFO', 0x1f: 'LC_REEXPORT_DYLIB', 0x20: 'LC_LAZY_LOAD_DYLIB',
    0x21: 'LC_ENCRYPTION_INFO', 0x22: 'LC_DYLD_INFO', 0x23: 'LC_DYLD_INFO_ONLY',
    0x24: 'LC_LOAD_UPWARD_DYLIB', 0x25: 'LC_VERSION_MIN_MACOSX', 0x26: 'LC_VERSION_MIN_IPHONEOS',
    0x27: 'LC_FUNCTION_STARTS', 0x28: 'LC_DYLD_ENVIRONMENT', 0x29: 'LC_MAIN',
    0x2a: 'LC_DATA_IN_CODE', 0x2b: 'LC_SOURCE_VERSION', 0x2c: 'LC_DYLIB_CODE_SIGN_DRS',
    0x2d: 'LC_ENCRYPTION_INFO_64', 0x2e: 'LC_LINKER_OPTION', 0x2f: 'LC_LINKER_OPTIMIZATION_HINT',
    0x30: 'LC_VERSION_MIN_TVOS', 0x31: 'LC_VERSION_MIN_WATCHOS', 0x32: 'LC_NOTE',
    0x33: 'LC_BUILD_VERSION', 0x34: 'LC_DYLD_EXPORTS_TRIE', 0x35: 'LC_DYLD_CHAINED_FIXUPS',
    0x36: 'LC_FILESET_ENTRY',
}
DYLIB_CMDS = (0xc, 0x18, 0x1f, 0x20, 0x24, 0xd)


def parse_cstr(buf, off):
    end = buf.index(b'\x00', off)
    return buf[off:end].decode('utf-8', 'replace')


def scan(path):
    with open(path, 'rb') as fh:
        data = fh.read()
    out = {'path': path, 'size': len(data)}
    magic = struct.unpack_from('<I', data, 0)[0]
    if magic not in (0xfeedfacf, 0xfeedface):
        out['error'] = 'not a thin Mach-O (magic=%08x)' % magic
        return out
    is64 = magic == 0xfeedfacf
    if is64:
        cputype, cpusub, ftype, ncmds, sizeofcmds, flags, res = struct.unpack_from('<iiIIIII', data, 4)
        hdr = 32
    else:
        cputype, cpusub, ftype, ncmds, sizeofcmds, flags = struct.unpack_from('<iiIIII', data, 4)
        hdr = 28
        res = 0
    out['header'] = {
        'magic': 'MH_MAGIC_64' if is64 else 'MH_MAGIC',
        'cputype': '0x%08x (%s)' % (cputype, CPU_TYPES.get(cputype, '?')),
        'cpusubtype': '0x%08x' % cpusub,
        'filetype': {2: 'MH_EXECUTE', 6: 'MH_DYLIB', 8: 'MH_BUNDLE', 1: 'MH_OBJECT'}.get(ftype, ftype),
        'ncmds': ncmds, 'sizeofcmds': sizeofcmds,
        'flags': '0x%08x [%s]' % (flags, ','.join(n for v, n in sorted(MH_FLAGS.items()) if flags & v)),
        'reserved': '0x%x' % res,
    }
    cmds = []
    off = hdr
    for _ in range(ncmds):
        cmd, cmdsize = struct.unpack_from('<II', data, off)
        name = LC.get(cmd, 'LC_0x%x' % cmd)
        entry = {'cmd': name, 'cmdsize': cmdsize, 'offset': off}
        if cmd in DYLIB_CMDS:
            noff = struct.unpack_from('<I', data, off + 8)[0]
            entry['name'] = parse_cstr(data, off + noff)
            ts, cs, cur = struct.unpack_from('<III', data, off + 8 + 4)
            entry['compat_version'] = '%d.%d.%d' % (cs >> 16, (cs >> 8) & 0xff, cs & 0xff)
            entry['current_version'] = '%d.%d.%d' % (ts >> 16, (ts >> 8) & 0xff, ts & 0xff)
        elif cmd in (0x1c, 0xd, 0x18, 0xf, 0x14):
            noff = struct.unpack_from('<I', data, off + 8)[0]
            entry['name'] = parse_cstr(data, off + noff)
        elif cmd in (0x21, 0x2d):
            cid, coff, csize = struct.unpack_from('<III', data, off + 8)
            entry.update({'cryptoff': coff, 'cryptsize': csize, 'cryptid': cid})
        elif cmd == 0x33:
            plat, minos, sdk, ntool = struct.unpack_from('<IIII', data, off + 8)
            entry.update({'platform': PLATFORMS.get(plat, plat),
                          'minos': '%d.%d.%d' % (minos >> 16, (minos >> 8) & 0xff, minos & 0xff),
                          'sdk': '%d.%d.%d' % (sdk >> 16, (sdk >> 8) & 0xff, sdk & 0xff),
                          'ntools': ntool})
        elif cmd == 0x19:
            segname = data[off + 8:off + 24].split(b'\x00')[0].decode()
            vmaddr, vmsize = struct.unpack_from('<QQ', data, off + 24)
            fileoff, filesize = struct.unpack_from('<QQ', data, off + 40)
            maxprot, initprot, nsects, segflags = struct.unpack_from('<IIII', data, off + 56)
            entry.update({'segname': segname, 'vmaddr': vmaddr, 'vmsize': vmsize,
                          'fileoff': fileoff, 'filesize': filesize,
                          'maxprot': '0x%x' % maxprot, 'initprot': '0x%x' % initprot,
                          'nsects': nsects, 'flags': segflags})
        elif cmd == 0x1d:
            doff, dsize = struct.unpack_from('<II', data, off + 8)
            entry.update({'dataoff': doff, 'datasize': dsize})
        elif cmd == 0x29:
            eoff, stack = struct.unpack_from('<QQ', data, off + 8)
            entry.update({'entryoff': eoff, 'stacksize': stack})
        elif cmd == 0x2b:
            v = struct.unpack_from('<Q', data, off + 8)[0]
            entry['version'] = '%d.%d.%d' % (v >> 40, (v >> 30) & 0x3ff, (v >> 20) & 0x3ff)
        elif cmd == 0x1b:
            entry['uuid'] = ''.join('%02X' % b for b in data[off + 8:off + 24])
        cmds.append(entry)
        if cmdsize == 0:
            break
        off += cmdsize
    out['load_commands'] = cmds

    # entitlements blob (LC_CODE_SIGNATURE -> CodeDirectory -> XML entitlements)
    out['entitlements'] = extract_entitlements(data, out['load_commands'])
    return out


def extract_entitlements(data, cmds):
    codesig = [c for c in cmds if c['cmd'] == 'LC_CODE_SIGNATURE']
    if not codesig:
        return None
    base, size = codesig[0]['dataoff'], codesig[0]['datasize']
    blob = data[base:base + size]
    if len(blob) < 12:
        return None
    magic, length, count = struct.unpack_from('>III', blob, 0)
    if magic != 0xfade0c02:
        return None
    off = 12
    for _ in range(count):
        m, l = struct.unpack_from('>II', blob, off)
        if m == 0xfade7171:  # CSMAGIC_EMBEDDED_ENTITLEMENTS
            return blob[off + 8:off + l].decode('utf-8', 'replace')
        if m == 0xfade7172:  # CSMAGIC_EMBEDDED_DER_ENTITLEMENTS
            return '<DER-encoded entitlements, %d bytes>' % (l - 8)
        off += l
    return None


def main():
    import json
    res = [scan(p) for p in sys.argv[1:]]
    print(json.dumps(res, indent=2, ensure_ascii=False))


if __name__ == '__main__':
    main()
