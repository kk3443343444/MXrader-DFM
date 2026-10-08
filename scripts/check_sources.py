#!/usr/bin/env python3
"""Structural sanity checks for the Rust / Swift / JS / shell sources.

Not a compiler — it catches what a truncated or half-written file looks like:
unbalanced braces (via a real lexer for comments/strings/char-literals/lifetimes
/raw-strings), stray tabs, leftover `todo!()`/`unimplemented!()`, CRLF, a missing
final newline, and `mod x;` declarations with no file behind them.
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
import re

ROOT = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else '.').resolve()
problems: list[str] = []


def lex_braces(text: str, js: bool = False, swift: bool = False) -> tuple[int, int, int]:
    """Return (open, close, paren_delta) after stripping comments/strings.

    `js=True` additionally blanks JavaScript regular-expression literals, which
    otherwise contribute braces and parens that make the counts meaningless.
    """
    i, n = 0, len(text)
    depth_open = depth_close = paren = 0
    prev_sig = ''  # last significant (non-space) character
    while i < n:
        c = text[i]
        # line comment
        if c == '/' and text.startswith('//', i):
            j = text.find('\n', i)
            i = n if j < 0 else j
            continue
        # block comment (nesting, as Rust allows)
        if c == '/' and text.startswith('/*', i):
            depth = 1
            i += 2
            while i < n and depth:
                if text.startswith('/*', i):
                    depth += 1
                    i += 2
                elif text.startswith('*/', i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            continue
        # JavaScript regex literal: '/' where an operand may start
        if js and c == '/' and (prev_sig == '' or prev_sig in '(,=:[!&|?{};+-*%~^<>'):
            i += 1
            in_class = False
            while i < n:
                if text[i] == '\\':
                    i += 2
                    continue
                if text[i] == '[':
                    in_class = True
                elif text[i] == ']':
                    in_class = False
                elif text[i] == '/' and not in_class:
                    i += 1
                    break
                elif text[i] == '\n':
                    break
                i += 1
            while i < n and re.match(r'[a-z]', text[i]):
                i += 1  # flags
            prev_sig = '/'
            continue
        # raw string: r"..." r#"..."# r##"..."##
        m = re.match(r'(?:b)?r(#{0,8})"', text[i:])
        if m:
            hashes = m.group(1)
            start = i + m.end()
            end_marker = '"' + hashes
            j = text.find(end_marker, start)
            i = n if j < 0 else j + len(end_marker)
            continue
        # normal string / byte string / JS template string.
        # NOTE: a bare `'` is handled below as a Rust char-literal/lifetime, so
        # it must not enter this branch (that would swallow lifetimes and the
        # braces after them).
        if c == '"' or (c == 'b' and text.startswith('b"', i)) or (js and (c == "'" or c == '`')):
            quote = '"' if c == 'b' else c
            if c == 'b':
                i += 1
                quote = '"'
            i += 1
            while i < n:
                if text[i] == '\\':
                    i += 2
                    continue
                if quote == '`' and text[i] == '$' and text.startswith('{', i + 1):
                    # template interpolation: keep the braces counted
                    depth_open += 1
                    i += 2
                    continue
                if text[i] == quote:
                    i += 1
                    break
                i += 1
            continue
        # char literal vs lifetime (Rust); Swift has neither - its `'` is just an
        # apostrophe inside a string literal, which is already consumed above.
        if c == "'" and swift:
            i += 1
            prev_sig = "'"
            continue
        if c == "'":
            nxt = text[i + 1:i + 3]
            if re.match(r"[A-Za-z_][A-Za-z0-9_]*", nxt) and not re.match(
                r"[A-Za-z_][A-Za-z0-9_]*'", nxt
            ):
                i += 1
                while i < n and re.match(r'[A-Za-z0-9_]', text[i]):
                    i += 1
                continue
            i += 1
            while i < n:
                if text[i] == '\\':
                    i += 2
                    continue
                if text[i] == "'":
                    i += 1
                    break
                i += 1
            continue
        if c == '{':
            depth_open += 1
        elif c == '}':
            depth_close += 1
        elif c == '(':
            paren += 1
        elif c == ')':
            paren -= 1
        if not c.isspace():
            prev_sig = c
        i += 1
    return depth_open, depth_close, paren


def check_rust(p: pathlib.Path, text: str) -> None:
    rel = p.relative_to(ROOT)
    o, c, paren = lex_braces(text)
    if o != c:
        problems.append(f'{rel}: unbalanced braces ({o} open vs {c} close)')
    if paren != 0:
        problems.append(f'{rel}: unbalanced parentheses (delta {paren:+d})')
    for pat, label in (
        (r'\btodo!\s*\(', 'todo!()'),
        (r'\bunimplemented!\s*\(', 'unimplemented!()'),
        (r'\bdbg!\s*\(', 'dbg!()'),
    ):
        n = len(re.findall(pat, text))
        if n:
            problems.append(f'{rel}: {n}× {label} left in the source')
    if '\t' in text:
        problems.append(f'{rel}: contains tab indentation')


def check_common(p: pathlib.Path, text: str) -> None:
    rel = p.relative_to(ROOT)
    if '\r\n' in text:
        problems.append(f'{rel}: CRLF line endings')
    if text and not text.endswith('\n'):
        problems.append(f'{rel}: no trailing newline')


def check_swift(p: pathlib.Path, text: str) -> None:
    """Swift structural check: same lexer, plus multi-line string handling.

    This does not type-check anything - it catches the failure mode that would
    otherwise only surface on a macOS CI run: a truncated or half-written file
    with unbalanced braces.
    """
    rel = p.relative_to(ROOT)
    o, c, paren = lex_braces(text, swift=True)
    if o != c:
        problems.append(f'{rel}: unbalanced braces ({o} open vs {c} close)')
    if paren != 0:
        problems.append(f'{rel}: unbalanced parentheses (delta {paren:+d})')
    for pat, label in (
        (r'\bTODO\(', 'TODO('),
        (r'\bfatalError\(', 'fatalError('),
    ):
        n = len(re.findall(pat, text))
        if n:
            problems.append(f'{rel}: {n}x {label} left in the source')
    if '\t' in text:
        problems.append(f'{rel}: contains tab indentation')


def check_js(p: pathlib.Path, text: str) -> None:
    rel = p.relative_to(ROOT)
    o, c, paren = lex_braces(text, js=True)
    if o != c:
        problems.append(f'{rel}: unbalanced braces ({o} vs {c})')
    if paren != 0:
        problems.append(f'{rel}: unbalanced parentheses (delta {paren:+d})')


def check_mods() -> None:
    src = ROOT / 'core/src'
    if not src.is_dir():
        return
    for f in src.rglob('*.rs'):
        for m in re.finditer(r'^\s*(?:pub\s+)?mod\s+([a-z_][a-z0-9_]*)\s*;',
                             f.read_text(encoding='utf-8'), re.M):
            name = m.group(1)
            base = f.parent
            if not ((base / f'{name}.rs').exists() or (base / name / 'mod.rs').exists()):
                problems.append(f'{f.relative_to(ROOT)}: mod {name} has no file')


def main() -> int:
    exts = {'.rs': check_rust, '.js': check_js, '.swift': check_swift}
    plain = ('.swift', '.css', '.html', '.sh', '.py', '.toml', '.yml', '.json', '.md', '.h')
    files = [
        p for p in ROOT.rglob('*')
        if p.is_file() and p.suffix in set(exts) | set(plain)
        and 'target' not in p.parts and '__pycache__' not in p.parts
    ]
    for p in files:
        try:
            text = p.read_text(encoding='utf-8')
        except (UnicodeDecodeError, OSError):
            problems.append(f'{p.relative_to(ROOT)}: not valid UTF-8')
            continue
        # assets extracted from the reference binary keep their original bytes
        if p.parent.name == 'assets' and p.suffix in ('.json', '.html'):
            continue
        check_common(p, text)
        fn = exts.get(p.suffix)
        if fn:
            fn(p, text)
    check_mods()

    print(f'files checked: {len(files)}')
    if problems:
        print(f'{len(problems)} problem(s):')
        for x in problems[:60]:
            print('  *', x)
        return 1
    print('structural sanity: clean')
    return 0


if __name__ == '__main__':
    sys.exit(main())
