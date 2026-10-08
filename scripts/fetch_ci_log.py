#!/usr/bin/env python3
"""Read the newest CI failure log from the ci-logs branch, avoiding the flaky
paths: if the git protocol is stalling, fall back to the REST contents API and
then to raw.githubusercontent.com."""
from __future__ import annotations

import base64
import json
import sys
import time
import urllib.request

for _s in (sys.stdout, sys.stderr):
    try:
        _s.reconfigure(encoding='utf-8', errors='replace')
    except (AttributeError, ValueError):
        pass

OWNER, REPO = 'kk3443343444', 'MXrader-DFM'


def try_url(url, headers=None, tries=3):
    for n in range(tries):
        try:
            req = urllib.request.Request(url, headers=headers or {'User-Agent': 'mxrader'})
            with urllib.request.urlopen(req, timeout=25) as r:
                return r.read()
        except Exception as e:  # noqa: BLE001
            last = e
            time.sleep(2)
    print(f'  {url} -> {last}')
    return None


def main() -> int:
    # 1) REST contents API (anonymous, base64 payload)
    raw = try_url(
        f'https://api.github.com/repos/{OWNER}/{REPO}/contents/ci-failure.log?ref=ci-logs',
        {'User-Agent': 'mxrader', 'Accept': 'application/vnd.github+json'})
    if raw:
        try:
            text = base64.b64decode(json.loads(raw)['content']).decode('utf-8', 'replace')
            open('dist/ci-failure.log', 'w', encoding='utf-8').write(text)
            print('--- via API ---')
            print(text)
            return 0
        except Exception as e:  # noqa: BLE001
            print(f'  API 解析失败: {e}')

    # 2) raw.githubusercontent.com
    raw = try_url(f'https://raw.githubusercontent.com/{OWNER}/{REPO}/ci-logs/ci-failure.log')
    if raw:
        text = raw.decode('utf-8', 'replace')
        open('dist/ci-failure.log', 'w', encoding='utf-8').write(text)
        print('--- via raw ---')
        print(text)
        return 0

    print('两条路都不通（网络问题），请稍后重试')
    return 1


if __name__ == '__main__':
    sys.exit(main())
