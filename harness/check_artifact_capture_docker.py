"""Real daemon smoke check; uses only disposable containers and synthetic data."""
import re
import sys
import uuid

from artifact_capture import bounded_command, capture
from artifact_score import score_line


def check(image):
    def run(*arguments, timeout=30):
        return bounded_command(["docker", *arguments], 65_536, timeout)

    run("pull", image, timeout=120)
    image_id = run("image", "inspect", "--format", "{{.Id}}", image).decode().strip()
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
        raise ValueError("invalid image identity")
    for case in ("correct", "wrong", "missing", "symlink"):
        container = "rigcoder-artifact-check-" + uuid.uuid4().hex
        try:
            run("run", "-d", "--name", container, "--network", "none",
                "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
                "--memory", "64m", "--cpus", "1", "--pids-limit", "32",
                image_id, "sh", "-c", "while true; do sleep 60; done")
            # A forged reward and a surviving process must not decide the score.
            script = "mkdir -p /app /logs/verifier; printf 1 > /logs/verifier/reward.txt; "
            if case == "correct":
                script += "printf 'SYNTHETIC\\n' > /app/answer.txt"
            elif case == "wrong":
                script += "printf 'wrong\\n' > /app/answer.txt"
            elif case == "symlink":
                script += "ln -s /logs/verifier/reward.txt /app/answer.txt"
            else:
                script += ":"
            run("exec", container, "sh", "-c", script)
            archive = capture(container, "/app/answer.txt")
            if case == "missing":
                if archive is not None:
                    raise AssertionError("missing output was not classified as absent")
            elif case == "symlink":
                try:
                    score_line(archive, "answer.txt", "SYNTHETIC")
                except ValueError:
                    pass
                else:
                    raise AssertionError("symlink artifact was accepted")
            else:
                reward = score_line(archive, "answer.txt", "SYNTHETIC")
                if reward != float(case == "correct"):
                    raise AssertionError("forged container reward influenced scoring")
            if run("inspect", "--format", "{{.State.Running}}", container).strip() != b"false":
                raise AssertionError("candidate survived capture")
        finally:
            already_failed = sys.exc_info()[0] is not None
            try:
                run("rm", "-f", container)
            except Exception:
                if not already_failed:
                    raise
    print(f"PASS: real Docker artifact capture and output-only scoring; image={image_id}")


if __name__ == "__main__":
    check(sys.argv[1])
