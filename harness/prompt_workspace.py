"""Fresh input copies and bounded prompt-only collection for the trusted launcher.

Collection must follow termination of the sandboxed improver. It returns bytes;
it never mutates the repository or decides whether a candidate should be kept.
"""
from dataclasses import dataclass
import hashlib
import os
from pathlib import Path
import stat

PROMPT = Path("crates/rigcoder/src/prompt.md")
MAX_PROMPT = 131_072
MAX_REPORT = 4_194_304
MAX_NOTE = 65_536


def checked_text(value, maximum):
    if not isinstance(value, bytes) or len(value) > maximum:
        raise ValueError("invalid prompt workspace input size")
    value.decode("utf-8")
    return value


def read_regular(root, relative, maximum):
    """Walk directory descriptors without following links; never block on a FIFO."""
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    directory = os.open(root, flags)
    try:
        for part in relative.parts[:-1]:
            child = os.open(part, flags, dir_fd=directory)
            os.close(directory)
            directory = child
        file = os.open(relative.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
        try:
            meta = os.fstat(file)
            if not stat.S_ISREG(meta.st_mode) or meta.st_nlink != 1 or meta.st_size > maximum:
                raise ValueError("proposal must be a bounded, unlinked regular file")
            with os.fdopen(file, "rb", closefd=False) as source:
                value = source.read(maximum + 1)
            if len(value) != meta.st_size or os.fstat(file).st_mtime_ns != meta.st_mtime_ns:
                raise ValueError("proposal changed during collection")
            return checked_text(value, maximum)
        finally:
            os.close(file)
    finally:
        os.close(directory)


@dataclass(frozen=True)
class PromptWorkspace:
    path: Path
    baseline_sha256: str
    report_sha256: str

    @classmethod
    def create(cls, destination, prompt, report):
        prompt = checked_text(prompt, MAX_PROMPT)
        report = checked_text(report, MAX_REPORT)
        destination = Path(destination)
        destination.mkdir(mode=0o700)  # Existing state must never be reused.
        (destination / PROMPT.parent).mkdir(parents=True)
        for relative, data in [(PROMPT, prompt), (Path("development.md"), report)]:
            with (destination / relative).open("xb") as output:
                output.write(data)
        return cls(destination.resolve(), hashlib.sha256(prompt).hexdigest(),
                   hashlib.sha256(report).hexdigest())

    def collect(self):
        prompt = read_regular(self.path, PROMPT, MAX_PROMPT)
        try:
            note = read_regular(self.path, Path("improvement.md"), MAX_NOTE)
        except FileNotFoundError:
            note = None
        return {"prompt": prompt, "note": note}
