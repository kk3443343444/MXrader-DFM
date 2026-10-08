#!/usr/bin/env python3
"""Deep Mach-O dump: full load commands, sections, symbols, ObjC/Swift names,
C strings, and a machine-readable section map for structure replication."""
import struct
import sys
import re
import json
from collections import Counter

LC_FULL = {
    0x80000018: 'LC_LOAD_WEAK_DYLIB(REQ)',
    0x8000001f: 'LC_REEXPORT_DYLIB(REQ)',
    0x8000001c: 'LC_RPATH(REQ)',
    0x80000033: 'LC_BUILD_VERSION',
    0x80000034: 'LC_DYLD_EXPORTS_TRIE',
    0x80000028: 'LC_DYLD_ENVIRONMENT(REQ)',
}


def cstr(buf, off, maxlen=4096):
    end = buf.find(b'\x00', off, off + maxlen)
    if end < 0:
        end = off + maxlen
    return buf[off:end].decode('utf-8', 'replace')


def parse(path):
    data = open(path, 'rb').read()
    res = {'sections': [], 'dylibs': [], 'rpaths': [], 'commands': []}
    magic = struct.unpack_from('<I', data, 0)[0]
    if magic != 0xfeedfacf:
        raise SystemExit('not thin arm64 Mach-O')
    cputype, cpusub, ftype, ncmds, sizeofcmds, flags, _ = struct.unpack_from('<iiIIIII', data, 4)
    res['header'] = {'cputype': cputype, 'ftype': ftype, 'ncmds': ncmds,
                     'sizeofcmds': sizeofcmds, 'flags': '0x%08x' % flags}
    off = 32
    symtab = None
    for _ in range(ncmds):
        cmd, cmdsize = struct.unpack_from('<II', data, off)
        name = LC_FULL.get(cmd, hex(cmd))
        entry = {'cmd': name, 'cmdsize': cmdsize}
        if cmd in (0xc, 0x18, 0x1f, 0x20, 0x24, 0x80000018, 0x8000001f, 0xd, 0x80000018):
            noff = struct.unpack_from('<I', data, off + 8)[0]
            entry['name'] = cstr(data, off + noff)
            res['dylibs'].append((entry['cmd'], entry['name']))
        elif cmd in (0x1c, 0x8000001c):
            noff = struct.unpack_from('<I', data, off + 8)[0]
            entry['name'] = cstr(data, off + noff)
            res['rpaths'].append(entry['name'])
        elif cmd == 0x19:
            seg = data[off + 8:off + 24].split(b'\x00')[0].decode()
            entry['segname'] = seg
            nsects = struct.unpack_from('<I', data, off + 64)[0]
            s = off + 72
            secs = []
            for _i in range(nsects):
                sname = data[s:s + 16].split(b'\x00')[0].decode()
                sgname = data[s + 16:s + 32].split(b'\x00')[0].decode()
                addr, size = struct.unpack_from('<QQ', data, s + 32)
                soff, align, reloff, nreloc, sflags = struct.unpack_from('<IIIII', data, s + 48)
                secs.append({'sect': sname, 'seg': sgname, 'addr': addr, 'size': size,
                             'offset': soff, 'align': align, 'flags': '0x%08x' % sflags})
                s += 80
            entry['sections'] = secs
            res['sections'].extend(secs)
        elif cmd == 0x2:
            symoff, nsyms, stroff, strsize = struct.unpack_from('<IIII', data, off + 8)
            symtab = (symoff, nsyms, stroff, strsize)
            entry.update({'symoff': symoff, 'nsyms': nsyms, 'stroff': stroff, 'strsize': strsize})
        elif cmd == 0x33 or cmd == 0x80000033:
            plat, minos, sdk, ntool = struct.unpack_from('<IIII', data, off + 8)
            entry.update({'platform': plat, 'minos': minos, 'sdk': sdk, 'ntools': ntool})
        elif cmd == 0x26:
            v, s = struct.unpack_from('<II', data, off + 8)
            entry.update({'version': v, 'sdk': s})
        elif cmd == 0x21 or cmd == 0x2d:
            cid, coff, csize = struct.unpack_from('<III', data, off + 8)
            entry.update({'cryptid': cid, 'cryptoff': coff, 'cryptsize': csize})
        res['commands'].append(entry)
        if cmdsize == 0:
            break
        off += cmdsize

    # ---- symbols ----
    syms = []
    if symtab:
        symoff, nsyms, stroff, strsize = symtab
        for i in range(nsyms):
            base = symoff + i * 16
            if base + 16 > len(data):
                break
            n_strx, n_type, n_sect, n_desc, n_value = struct.unpack_from('<IBBHQ', data, base)
            if n_strx == 0 or stroff + n_strx >= len(data):
                continue
            nm = cstr(data, stroff + n_strx)
            syms.append({'name': nm, 'type': n_type, 'sect': n_sect, 'value': n_value})
    res['nsyms'] = len(syms)
    res['symbols'] = syms

    # ---- strings from __cstring / __objc_methname / __swift5_* ----
    strs = {}
    for sec in res['sections']:
        if sec['sect'] in ('__cstring', '__objc_methname', '__objc_classname',
                           '__objc_methtype', '__swift5_fieldmd', '__swift5_reflstr',
                           '__swift5_typeref', '__const'):
            blob = data[sec['offset']:sec['offset'] + sec['size']]
            found = re.findall(rb'[\x20-\x7e]{4,200}', blob)
            strs[sec['sect']] = [s.decode() for s in found]
    res['strings'] = strs
    return res


def main():
    r = parse(sys.argv[1])
    out = sys.argv[2] if len(sys.argv) > 2 else None
    if out:
        with open(out, 'w', encoding='utf-8') as fh:
            json.dump(r, fh, ensure_ascii=False, indent=1)
    print('header:', r['header'])
    print('dylibs:', len(r['dylibs']))
    for k, v in r['dylibs']:
        print('   ', k, v)
    print('rpaths:', r['rpaths'])
    print('sections:', len(r['sections']))
    print('symbols:', r['nsyms'])
    for k, v in r['strings'].items():
        print('strings', k, len(v))


if __name__ == '__main__':
    main()
