#!/usr/bin/env python3
"""Capture declared build inputs without mounting evaluation data or Git history."""

import hashlib
import json
from pathlib import Path
import shutil
import stat
import subprocess
import sys

INPUTS = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "crates")
BUILD_COMMAND = ["cargo", "build", "--locked", "--release", "-p", "rigcoder-cli"]


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def inventory(root):
    files = {}

    def visit(path):
        mode = path.lstat().st_mode
        entry = {"mode": stat.S_IMODE(mode)}
        if stat.S_ISREG(mode):
            entry.update(kind="file", sha256=digest(path))
        elif stat.S_ISDIR(mode):
            entry.update(kind="directory")
            for child in sorted(path.iterdir()):
                visit(child)
        else:
            raise ValueError(f"build inputs must be regular files/directories: {path}")
        files[path.relative_to(root).as_posix()] = entry

    for name in INPUTS:
        visit(root / name)
    return files


def snapshot(root, destination):
    def copy(source, target):
        mode = source.lstat().st_mode
        if stat.S_ISDIR(mode):
            target.mkdir(parents=True)
            for child in sorted(source.iterdir()):
                copy(child, target / child.name)
            shutil.copymode(source, target)
        elif stat.S_ISREG(mode):
            shutil.copy2(source, target)
        else:
            raise ValueError(f"build inputs must be regular files/directories: {source}")

    destination.mkdir(parents=True)
    for name in INPUTS:
        copy(root / name, destination / name)
    return {
        "version": 1,
        "source_head": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=root, text=True
        ).strip(),
        "inputs": inventory(destination),
    }


def verify(root, binary, receipt, architecture):
    record = json.loads(receipt.read_text())
    if record.get("version") != 1 or record.get("command") != BUILD_COMMAND:
        raise ValueError("unsupported build receipt")
    if record.get("architecture") != architecture:
        raise ValueError("build receipt architecture mismatch")
    image = record.get("builder_image", "")
    if not isinstance(image, str) or len(image) != 71 or not image.startswith("sha256:") or any(char not in "0123456789abcdef" for char in image[7:]):
        raise ValueError("build receipt requires an immutable builder image ID")
    if record.get("binary_sha256") != digest(binary):
        raise ValueError("build receipt binary mismatch")
    if record.get("inputs") != inventory(root):
        raise ValueError("build receipt source inputs mismatch")
    return record


if __name__ == "__main__":
    if sys.argv[1] == "snapshot":
        result = snapshot(Path(sys.argv[2]), Path(sys.argv[3]))
    elif sys.argv[1] == "receipt":
        result = json.loads(Path(sys.argv[2]).read_text())
        result.update(
            binary_sha256=digest(Path(sys.argv[3])),
            architecture=sys.argv[4],
            builder_image=sys.argv[5],
            command=BUILD_COMMAND,
        )
    elif sys.argv[1] == "verify":
        result = verify(Path(sys.argv[2]), Path(sys.argv[3]), Path(sys.argv[4]), sys.argv[5])
    else:
        raise ValueError("expected snapshot or receipt")
    print(json.dumps(result, indent=2, sort_keys=True))
