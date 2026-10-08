#!/usr/bin/env python3
"""Heuristic Swift mangled-name tokenizer.

Turns `_$s18BattleReceiverOpen6EngineC5startyyF` into
[BattleReceiverOpen, Engine(C), start(f)] so the app's internal
type/member layout can be reconstructed without a full demangler.
"""
import json
import re
import sys
import collections

KIND = {
    'C': 'class', 'V': 'struct', 'O': 'enum', 'P': 'protocol',
    'f': 'func', 'F': 'func', 'v': 'var', 'p': 'prop', 'M': 'method',
    'W': 'witness', 'T': 'typealias', 'a': 'typealias', 'i': 'subscript',
    'c': 'init', 'd': 'deinit', 'Z': 'static', 'S': 'static',
}


def tokenize(body):
    toks = []
    i = 0
    n = len(body)
    while i < n:
        c = body[i]
        if c.isdigit():
            j = i
            while j < n and body[j].isdigit():
                j += 1
            ln = int(body[i:j])
            ident = body[j:j + ln]
            if len(ident) < ln:
                toks.append(('!trunc', ident))
                break
            toks.append(('id', ident))
            i = j + ln
        else:
            toks.append(('kind', c))
            i += 1
    return toks


def humanize(toks):
    """Render token list into a readable dotted path plus kind tags."""
    out = []
    for t, v in toks:
        if t == 'id':
            out.append(v)
        elif t == 'kind':
            k = KIND.get(v)
            if k and out:
                out[-1] = out[-1]
            out.append('<%s:%s>' % (k, v) if k else v)
    return '.'.join(out)


def main():
    path = sys.argv[1]
    d = json.load(open(path, encoding='utf-8'))
    syms = [s['name'] for s in d['symbols']]

    types = {}          # nominal type name -> {'kind':..., 'members': set()}
    rendered = []
    for s in syms:
        if not s.startswith('_$s'):
            continue
        body = s[3:]
        toks = tokenize(body)
        if not toks:
            continue
        ids = [v for t, v in toks if t == 'id']
        if not ids or ids[0] != 'BattleReceiverOpen':
            continue
        # nominal type = first identifier after module that is followed by a kind letter
        cur_type = None
        for idx, (t, v) in enumerate(toks):
            if t == 'kind' and v in ('C', 'V', 'O', 'P', 'E', 'e') and idx >= 2:
                prev = toks[idx - 1]
                if prev[0] == 'id':
                    nm = prev[1]
                    ext = v in ('E', 'e')
                    rec = types.setdefault(nm, {'kind': KIND.get(v, v) + ('-extension' if ext else ''),
                                                'members': set()})
                    cur_type = nm
            elif t == 'id' and cur_type and idx >= 2:
                types[cur_type]['members'].add(v)
        rendered.append(humanize(toks))

    print('== nominal types (module BattleReceiverOpen) ==')
    for nm, rec in sorted(types.items(), key=lambda kv: -len(kv[1]['members'])):
        mem = sorted(m for m in rec['members'] if m != nm)
        print('\n  %s  [%s]  members=%d' % (nm, rec['kind'], len(mem)))
        if mem:
            print('     ' + ', '.join(mem))
    print('\n== total rendered symbols:', len(rendered))
    out = sys.argv[2] if len(sys.argv) > 2 else None
    if out:
        with open(out, 'w', encoding='utf-8') as fh:
            fh.write('\n'.join(sorted(set(rendered))))


if __name__ == '__main__':
    main()
