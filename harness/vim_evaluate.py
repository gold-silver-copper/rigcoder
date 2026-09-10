"""Trusted captured-artifact entry point embedded into the benchmark runner."""
import base64
import hashlib
import json
from pathlib import Path
import sys
import time

from artifact_capture import capture
from artifact_score import read_artifact
from vim_execute import MAX_CSV, evaluate_macros
from vim_script import MAX_SCRIPT


def evaluate(container, image, spec, evidence, timeout):
    evidence = Path(evidence)
    deadline = time.monotonic() + timeout
    hashes = {}
    captured = {}
    for name, limit in [('apply_macros.vim', MAX_SCRIPT), ('input.csv', MAX_CSV - 2048)]:
        archive = capture(container, '/app/' + name, deadline - time.monotonic(),
                          max_archive=min(MAX_CSV, limit + 65_536))
        hashes[name] = None
        if archive is None:
            captured[name] = None
            continue
        with (evidence / (name + '.tar')).open('xb') as output:
            output.write(archive)
        hashes[name] = hashlib.sha256(archive).hexdigest()
        captured[name] = read_artifact(archive, name, max_artifact=limit, max_archive=MAX_CSV)
    with (evidence/'vim-executions.jsonl').open('x') as executions:
        def record(event):
            event = {**event, 'stdout':base64.b64encode(event['stdout']).decode('ascii'),
                     'stderr':base64.b64encode(event['stderr']).decode('ascii'),
                     'stdout_encoding':'base64', 'stderr_encoding':'base64'}
            executions.write(json.dumps(event, sort_keys=True)+'\n')
            executions.flush()
        if any(value is None for value in captured.values()):
            reward = 0.0
        else:
            reward = evaluate_macros(captured['apply_macros.vim'], captured['input.csv'],
                                     spec['expected_sha256'], image, deadline-time.monotonic(),
                                     record=record)
    result = {'scorer':'vim_macros_v1', 'reward':reward, 'image':image,
              'archive_sha256':hashes, 'expected_sha256':spec['expected_sha256']}
    with (evidence/'vim-result.json').open('x') as output:
        json.dump(result, output, sort_keys=True)
    return reward


if __name__ == '__main__':
    try:
        print(evaluate(sys.argv[1], sys.argv[2], json.load(sys.stdin), sys.argv[3], int(sys.argv[4])))
    except Exception:
        print('Vim evaluation failed; inspect retained evidence', file=sys.stderr)
        raise SystemExit(1)
