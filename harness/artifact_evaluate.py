"""Host-side entry point embedded in rigcoder-bench for output-line tasks."""
import hashlib
import json
from pathlib import Path
import sys

from artifact_capture import capture
from artifact_score import score_line


def evaluate(container, spec, evidence, timeout):
    evidence = Path(evidence)
    archive = capture(container, spec["artifact"], timeout)
    if archive is None:
        reward = 0.0
        digest = None
    else:
        with (evidence / "artifact.tar").open("xb") as output:
            output.write(archive)
        reward = score_line(archive, Path(spec["artifact"]).name, spec["expected"])
        digest = hashlib.sha256(archive).hexdigest()
    record = {"scorer": "output_line_v1", "reward": reward,
              "artifact_missing": archive is None, "archive_sha256": digest}
    with (evidence / "output-line-result.json").open("x") as output:
        json.dump(record, output, sort_keys=True)
    return reward


if __name__ == "__main__":
    try:
        spec = json.load(sys.stdin)
        print(evaluate(sys.argv[1], spec, sys.argv[2], int(sys.argv[3])))
    except Exception:
        # No candidate data or expected answer enters an exception diagnostic.
        print("output-line evaluation failed; inspect retained capture", file=sys.stderr)
        raise SystemExit(1)
