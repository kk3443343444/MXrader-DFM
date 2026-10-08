#!/usr/bin/env python3
"""Detect double-encoded (UTF-8 bytes re-decoded as GBK) text.

Works on CJK *runs* rather than whole lines: a run of >=3 CJK characters that
GBK-encodes into bytes which decode back as UTF-8 Chinese is almost certainly
double-encoded. This catches damage that whole-line round-trips miss (because one
unrecoverable byte anywhere on the line aborts the line-level test).

Usage:
    python find_double_encoded.py [--fix] <dir or file> ...
"""
import re
import sys
# Non-UTF-8 consoles (Windows Chinese default is cp936) raise UnicodeEncodeError on
# decorative glyphs such as U+2194 or U+2705. Keep the console encoding so Chinese still
# renders, and downgrade only the characters it cannot represent.
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding='utf-8', errors='replace')  # type: ignore[attr-defined]
    except (AttributeError, ValueError):
        pass
import pathlib

SUFFIXES = ('.rs', '.swift', '.sh', '.md', '.json', '.js', '.html', '.css', '.toml', '.yml', '.plist')
CJK_RUN = re.compile(r'[\u3000-\u303f\u4e00-\u9fff\uff00-\uffef，。；：（）【】、·]+')
# Characters that legitimately appear often in Chinese; if a run consists ONLY of
# these we do not flag it (guards against coincidental round-trips).
COMMON = set('的了是在我有和就不人都一个上也很到说要去你会着没有看好自己这')


def recover(text: str):
    try:
        raw = text.encode('gbk')
    except UnicodeEncodeError:
        return None
    try:
        fixed = raw.decode('utf-8')
    except UnicodeDecodeError:
        return None
    if sum(1 for c in fixed if '\u4e00' <= c <= '\u9fff') < 2:
        return None
    return fixed


def scan_line(line: str):
    """Return list of (before, after) for damaged runs on this line."""
    hits = []
    for m in CJK_RUN.finditer(line):
        run = m.group(0)
        if len(run) < 3:
            continue
        if all(c in COMMON for c in run):
            continue
        fixed = recover(run)
        if fixed and fixed != run:
            hits.append((run, fixed))
    return hits


def main() -> int:
    fix = '--fix' in sys.argv
    roots = [a for a in sys.argv[1:] if not a.startswith('--')]
    total = 0
    for root in roots:
        base = pathlib.Path(root)
        files = [base] if base.is_file() else [
            p for p in base.rglob('*')
            if p.is_file() and p.suffix in SUFFIXES
            and 'target' not in p.parts and '__pycache__' not in p.parts
        ]
        for p in files:
            try:
                text = p.read_text(encoding='utf-8')
            except (UnicodeDecodeError, OSError):
                print(f'{p}: not valid UTF-8')
                total += 1
                continue
            lines = text.splitlines(keepends=True)
            out, file_hits = [], []
            for i, line in enumerate(lines, 1):
                hits = scan_line(line)
                if hits:
                    file_hits.append((i, hits))
                    for before, after in hits:
                        line = line.replace(before, after)
                out.append(line)
            if file_hits:
                total += len(file_hits)
                print(f'\n{p}  ({len(file_hits)} damaged lines)')
                for i, hits in file_hits[:6]:
                    for before, after in hits[:3]:
                        print(f'  {i}: {before[:70]}')
                        print(f'     -> {after[:70]}')
                if fix:
                    p.write_text(''.join(out), encoding='utf-8')
                    print('  [fixed]')
    print(f'\ndamaged lines: {total}')
    return 0 if total == 0 else 1


if __name__ == '__main__':
    sys.exit(main())
