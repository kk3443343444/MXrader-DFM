#!/usr/bin/env python3
"""Watch a GitHub Actions run WITHOUT using the REST API (anonymous API is rate
limited to 60 req/hour, which a build loop burns through).

Two unauthenticated signals that do not count against that quota:

  1. the workflow status badge SVG  -> "passing" / "failing" / no status
  2. the `ci-logs` branch tip via `git ls-remote`; the workflow force-pushes a
     commit to it on every failure, so a new SHA means "the newest run failed"

When it sees a failure it fetches that branch with plain git and prints
ci-failure.log (also no API).
"""
from __future__ import annotations

import subprocess
import sys
import time
import urllib.request

for _s in (sys.stdout, sys.stderr):
    try:
        _s.reconfigure(encoding='utf-8', errors='replace')
    except (AttributeError, ValueError):
        pass

REPO = 'https://github.com/kk3443343444/MXrader-DFM.git'
BADGE = 'https://github.com/kk3443343444/MXrader-DFM/actions/workflows/ios.yml/badge.svg'
WORK = '.'


def sh(args, timeout=60):
    return subprocess.run(args, cwd=WORK, capture_output=True, text=True,
                          timeout=timeout, encoding='utf-8', errors='replace')


def ci_logs_sha() -> str:
    r = sh(['git', 'ls-remote', REPO, 'ci-logs'])
    for line in (r.stdout or '').splitlines():
        if line.endswith('refs/heads/ci-logs'):
            return line.split()[0]
    return ''


def badge_state() -> str:
    try:
        with urllib.request.urlopen(BADGE, timeout=20) as r:
            svg = r.read().decode('utf-8', 'replace')
    except Exception as e:  # noqa: BLE001
        return f'error: {e}'
    for token in ('passing', 'failing', 'no status'):
        if token in svg:
            return token
    return 'unknown'


def main() -> int:
    start_sha = ci_logs_sha()
    print(f'baseline ci-logs = {start_sha[:12] or "(none)"}')
    deadline = time.time() + 1500
    last = None
    stable = 0
    while time.time() < deadline:
        state = badge_state()
        now = ci_logs_sha()
        stamp = time.strftime('%H:%M:%S')
        if state != last:
            print(f'  {stamp} badge={state} ci-logs={now[:12] or "(none)"}')
        last = state
        if now != start_sha:
            print(f'\n*** 新失败：ci-logs 从 {start_sha[:12]} 变成 {now[:12]}，拉下来看日志 ***')
            # 远端每次失败都 force-push，所以 refspec 必须带前导 '+'（强制更新），
            # 否则本地已有的 ci-logs-watch ref 不是 fast-forward，git 会拒绝更新，
            # 于是拿到的是**上一轮**的旧日志（这个坑踩过一次）。
            r = sh(['git', 'fetch', '--depth', '1', '--force', REPO,
                    '+refs/heads/ci-logs:refs/heads/ci-logs-watch'])
            print(r.stdout or r.stderr)
            r = sh(['git', 'show', 'refs/heads/ci-logs-watch:ci-failure.log'])
            text = r.stdout or r.stderr
            open('dist/ci-failure.log', 'w', encoding='utf-8').write(text)
            print('================ ci-failure.log ================')
            print(text)
            return 2
        if state == 'passing':
            stable += 1
            if stable >= 2:
                print('\n*** badge=passing：流水线绿了 ***')
                return 0
        else:
            stable = 0
        time.sleep(30)
    print('等待超时')
    return 3


if __name__ == '__main__':
    sys.exit(main())
