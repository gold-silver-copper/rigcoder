"""Host scorer for captured polyglot directories and isolated functional runs."""
import base64
import hashlib
import json
from pathlib import Path
import sys
import time

from artifact_capture import capture
from polyglot_artifact import MAX_DIRECTORY_ARCHIVE, source_from_archive
from polyglot_execute import evaluate_source


def evaluate(container, image, evidence, timeout):
    evidence = Path(evidence)
    deadline = time.monotonic() + timeout
    archive = capture(container, '/app/polyglot', timeout, max_archive=MAX_DIRECTORY_ARCHIVE)
    digest = None
    if archive is not None:
        with (evidence / 'polyglot-artifact.tar').open('xb') as output:
            output.write(archive)
        digest = hashlib.sha256(archive).hexdigest()
    source = source_from_archive(archive)
    with (evidence / 'polyglot-executions.jsonl').open('x') as executions:
        def record(event):
            event = {**event, 'stdout': base64.b64encode(event['stdout']).decode('ascii'),
                     'stderr': base64.b64encode(event.get('stderr', b'')).decode('ascii'),
                     'stdout_encoding': 'base64', 'stderr_encoding': 'base64'}
            executions.write(json.dumps(event, sort_keys=True) + '\n')
            executions.flush()

        if source is None:
            reward = 0.0
        else:
            reward, _ = evaluate_source(source, image, deadline - time.monotonic(), record=record)
    result = {'scorer': 'polyglot_v1', 'reward': reward, 'archive_sha256': digest,
              'source_sha256': None if source is None else hashlib.sha256(source).hexdigest(),
              'image': image, 'artifact_missing': archive is None}
    with (evidence / 'polyglot-result.json').open('x') as output:
        json.dump(result, output, sort_keys=True)
    return reward


if __name__ == '__main__':
    try:
        print(evaluate(sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])))
    except Exception:
        print('polyglot evaluation failed; inspect retained evidence', file=sys.stderr)
        raise SystemExit(1)
