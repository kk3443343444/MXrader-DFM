#!/usr/bin/env python3
"""Detect GBK-artifact mojibake in source files.

Two signals, either of which flags a line:

1. *Round-trip*: `line.encode('gbk').decode('utf-8')` succeeds and the result
   itself contains Chinese. Very precise, but misses lines where the damage
   already lost bytes (they turn into '?').
2. *Rare-syllable signature*: the line contains a run of GBK-artifact
   syllables (锛 鐨 涓 鍜 鈥 鍏 ...). A whitelist removes the handful of
   legitimate characters that mojibake of common words can also produce
   (e.g. 设置 -> 璁剧疆 contains 疆, 数据 -> 鏁版嵁 contains 版).

Usage:
    python find_mojibake.py [--fix] <dir or file> ...
"""
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

# Rare CJK syllables produced by reading UTF-8 Chinese as GBK.
SIG = set(
    '锛鐨涓鍜鈥鍏鑸鏄鍣纭绋鎬閿夌閫鎵鍔鐜鍒鎯鐪璁鐢鏈堣浆鏋璇鎴鍦鏂鍑浣绛敤瓧娈靛祵彇鍗曞厓鍚屾椂甯搁噺杩欎釜'
    '鏈夋晥鎬ц兘棰勭暀剧疆鐪熷疄鏁版嵁妫€鏌ユ帴鍙ｆ柟寮忚緭鍑鸿緭鍏ュ紑鍏虫祴璇曟棩蹇楄寖鍥'
)

# Characters that legitimately appear in Simplified Chinese *and* can be
# produced by mojibake of common words — never count them as evidence.
WHITELIST = set('版疆缓设数时本更多是不用文件生效实内主要求完整需要确认')

SUFFIXES = ('.rs', '.swift', '.sh', '.md', '.json', '.js', '.html', '.css', '.toml', '.yml')


def recover(text: str):
    try:
        return text.encode('gbk').decode('utf-8')
    except (UnicodeEncodeError, UnicodeDecodeError):
        return None


def evidence(line: str):
    """Return (kind, recovered) when the line looks damaged."""
    hits = [c for c in line if c in SIG and c not in WHITELIST]
    if len(hits) >= 2:
        return 'signature', recover(line)
    if hits and any(c in line for c in '锛鐨涓鍜鈥'):
        # a single, extremely rare syllable is enough with a strong anchor
        return 'signature', recover(line)
    fixed = recover(line)
    if fixed and sum(1 for c in fixed if '\u4e00' <= c <= '\u9fff') >= 2:
        return 'roundtrip', fixed
    return None


def main():
    fix = '--fix' in sys.argv
    roots = [a for a in sys.argv[1:] if not a.startswith('--')]
    total = 0
    for root in roots:
        base = pathlib.Path(root)
        files = [base] if base.is_file() else [
            p for p in base.rglob('*') if p.is_file() and p.suffix in SUFFIXES
        ]
        for p in files:
            try:
                text = p.read_text(encoding='utf-8')
            except (UnicodeDecodeError, OSError):
                continue
            lines = text.splitlines(keepends=True)
            hits, out = [], []
            for i, line in enumerate(lines, 1):
                body = line.rstrip('\r\n')
                eol = line[len(body):]
                ev = evidence(body)
                if ev:
                    hits.append((i, ev[0], body.strip(), ev[1]))
                    out.append((ev[1] if ev[1] else body) + eol)
                else:
                    out.append(line)
            if hits:
                total += len(hits)
                print(f'\n{p}  ({len(hits)} damaged lines)')
                for i, kind, before, after in hits[:8]:
                    print(f'  {i} [{kind}]: {before[:110]}')
                    if after:
                        print(f'     -> {after[:110]}')
                if fix:
                    p.write_text(''.join(out), encoding='utf-8')
                    print('  [fixed]')
    print(f'\ntotal damaged lines: {total}')
    return 0 if total == 0 else 1


if __name__ == '__main__':
    sys.exit(main())
