#!/usr/bin/env python3
"""Poll a GitHub Actions run and print the step conclusions + the ci-logs failure log.

Why Python and not the GitHub CLI: `gh` is not installed here, and the job-log API
needs auth (403). The workflow publishes its failure log to a branch, which the
contents API serves anonymously - so this script can read failures with no token.
"""
from __future__ import annotations

import base64
import json
import sys
import time
import urllib.error
import urllib.request

for _s in (sys.stdout, sys.stderr):
    try:
        _s.reconfigure(encoding='utf-8', errors='replace')
    except (AttributeError, ValueError):
        pass

OWNER = 'kk3443343444'
REPO = 'MXrader-DFM'
API = 'https://api.github.com'
UA = {'User-Agent': 'mxrader-ci-watch', 'Accept': 'application/vnd.github+json'}


def get(path: str, tries: int = 3):
    url = path if path.startswith('http') else API + path
    for n in range(tries):
        try:
            req = urllib.request.Request(url, headers=UA)
            with urllib.request.urlopen(req, timeout=25) as r:
                return json.loads(r.read().decode('utf-8'))
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            if n == tries - 1:
                raise
            time.sleep(3)
        except Exception:
            if n == tries - 1:
                raise
            time.sleep(3)
    return None


def main() -> int:
    deadline = time.time() + 1800
    seen = None
    while time.time() < deadline:
        runs = get(f'/repos/{OWNER}/{REPO}/actions/runs?per_page=1')
        if not runs:
            print('no runs yet')
            return 1
        run = runs['workflow_runs'][0]
        if seen != run['id']:
            seen = run['id']
            print(f"watching run #{run['run_number']} id={run['id']} ({run['event']})")
        print(f"  {time.strftime('%H:%M:%S')} status={run['status']} conclusion={run['conclusion']}")
        if run['status'] == 'completed':
            jobs = get(f"/repos/{OWNER}/{REPO}/actions/runs/{run['id']}/jobs")
            for j in jobs['jobs']:
                print(f"JOB {j['name']} -> {j['conclusion']}")
                for s in j['steps']:
                    mark = 'ok ' if s['conclusion'] == 'success' else (s['conclusion'] or '...')
                    print(f"   {mark:9} {s['name']}")
            print(f"URL https://github.com/{OWNER}/{REPO}/actions/runs/{run['id']}")

            f = get(f'/repos/{OWNER}/{REPO}/contents/ci-failure.log?ref=ci-logs')
            if f and f.get('content'):
                text = base64.b64decode(f['content']).decode('utf-8', 'replace')
                open('dist/ci-failure.log', 'w', encoding='utf-8').write(text)
                print('\n================ ci-failure.log ================')
                print(text)
            else:
                print('\n(no ci-failure.log on the ci-logs branch -> the run probably succeeded)')
            return 0 if run['conclusion'] == 'success' else 2
        time.sleep(20)
    print('timed out waiting for the run')
    return 3


if __name__ == '__main__':
    sys.exit(main())
