#!/usr/bin/env python3
"""显式验收 lexical HTTP、维护 worker 与 projection 恢复。"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time
import uuid
from acceptance import Journey, ROOT


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config-json', type=Path)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/inkcre')
    args = parser.parse_args()
    args.binary = args.binary.resolve(strict=True)
    args.extension_binary = None
    runtime = ROOT / '.runtime/acceptance'
    runtime.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=runtime) as directory:
        journey = Journey(args, Path(directory))
        check = subprocess.run([str(args.binary), 'db-check'], env=journey.env, capture_output=True, text=True, timeout=60)
        if check.returncode or json.loads(check.stdout).get('environment') != 'development':
            raise RuntimeError('requires a dedicated development database')
        journey.env.pop('INKCRE_NO_WORKER', None)
        marker = 'lexical-' + str(uuid.uuid4())
        def submit(rebuild=False):
            command = 'rebuild' if rebuild else 'maintain'
            job = journey.request('POST', '/api/v1/jobs', {'type': f'core.feature_retrieval.lexical.{command}.v1', 'parameters': {'options': {'max_records': 1000}}}, expected=201)
            journey.jobs.append(job['id'])
            deadline = time.monotonic() + 40
            while time.monotonic() < deadline:
                saved = journey.request('GET', f"/api/v1/jobs/{job['id']}")
                if saved['status'] not in ('pending', 'running'):
                    if saved['status'] != 'finished' or saved['state']['indexed'] < 1:
                        raise RuntimeError('lexical worker failed to persist projection report')
                    return
                time.sleep(0.1)
            raise RuntimeError('lexical worker acceptance deadline exceeded')
        try:
            journey.start()
            block = journey.request('POST', '/api/v1/blocks', {'resolver': 'core.text.v1', 'content': marker + ' literal_% 中文 Straße'}, expected=201)
            journey.blocks.append(block['id'])
            request = {'query': marker, 'filters': {'resolver': {'eq': 'core.text.v1'}}}
            journey.request('POST', '/api/v1/lexical-retrieval', {'query': '  '}, expected=422)
            journey.request('POST', '/api/v1/lexical-retrieval', {'query': marker, 'filters': {'resolver': None}}, expected=422)
            submit()
            def matches(payload):
                return journey.request('POST', '/api/v1/lexical-retrieval', payload)['matches']
            if not any(m['block']['id'] == block['id'] for m in matches(request)):
                raise RuntimeError('HTTP lexical retrieval lost maintained Block')
            if not any(m['block']['id'] == block['id'] for m in matches({'query': 'literal_%'})):
                raise RuntimeError('literal LIKE query was not preserved')
            journey.request('PATCH', f"/api/v1/blocks/{block['id']}", {'content': marker + ' changed'})
            if any(m['block']['id'] == block['id'] for m in matches(request)):
                raise RuntimeError('stale projection remained searchable')
            submit(True)
            if not any(m['block']['id'] == block['id'] for m in matches(request)):
                raise RuntimeError('rebuild did not recover projection')
            print(json.dumps({'ok': True, 'observations': ['HTTP参数准入', '真实维护与重建worker', '字面LIKE与过滤', '过期projection排除及恢复']}, ensure_ascii=False))
        finally:
            try:
                for id_ in journey.jobs:
                    saved = journey.request('GET', f'/api/v1/jobs/{id_}')
                    if saved['status'] in ('pending', 'running'):
                        journey.request('POST', f'/api/v1/jobs/{id_}/abort')
                        deadline = time.monotonic() + 20
                        while saved['status'] == 'running' and time.monotonic() < deadline:
                            time.sleep(0.1)
                            saved = journey.request('GET', f'/api/v1/jobs/{id_}')
            finally:
                journey.cleanup()
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except Exception:
        print(json.dumps({'ok': False, 'error': 'lexical acceptance failed; private diagnostics suppressed'}))
        raise SystemExit(1)
