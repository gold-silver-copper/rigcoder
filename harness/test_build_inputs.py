"""Exercise the real build wrapper with synthetic inputs and a Docker substitute."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class BuildInputs(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rigcoder-build-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "harness").mkdir()
        for name in ("build-linux.sh", "build-inputs.py"):
            shutil.copy2(Path(__file__).with_name(name), self.root / "harness" / name)
        for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
            (self.root / name).write_text("synthetic input\n")
        (self.root / "crates/cli/src").mkdir(parents=True)
        self.source = self.root / "crates/cli/src/main.rs"
        self.source.write_text("captured source\n")
        (self.root / "harness/holdout-canary").write_text("hidden evaluation input")
        for args in (("init", "-q"), ("add", "."), ("-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture")):
            subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)
        (self.root / "fakebin").mkdir()
        docker = self.root / "fakebin/docker"
        docker.write_text('''#!/usr/bin/env python3
import os, pathlib, sys
image = "sha256:" + "a" * 64
if sys.argv[1] == "pull":
    sys.exit(0)
if sys.argv[1] == "rm":
    sys.exit(0)
if sys.argv[1] == "build":
    assert sys.stdin.read().startswith("FROM rust@sha256:")
    pathlib.Path(sys.argv[sys.argv.index("--iidfile") + 1]).write_text(image)
    sys.exit(0)
if sys.argv[1] == "image":
    if any("RepoDigests" in arg for arg in sys.argv):
        print("rust@" + image)
        sys.exit(0)
    print(image + " linux/arm64")
    sys.exit(0)
assert image in sys.argv
mounts = [sys.argv[i+1] for i, arg in enumerate(sys.argv) if arg == "-v"]
src_mount = next(m for m in mounts if m.endswith(":/src:ro"))
src = pathlib.Path(src_mount.removesuffix(":/src:ro"))
assert not (src / "harness").exists()
assert not (src / ".git").exists()
if "fetch" in sys.argv:
    assert "--locked" in sys.argv
    sys.exit(0)
assert "--locked" in sys.argv[-1]
assert (src / "crates/cli/src/main.rs").read_text() == "captured source\\n"
pathlib.Path(os.environ["ORIGINAL_SOURCE"]).write_text("changed after capture\\n")
out = pathlib.Path(next(m for m in mounts if m.endswith(":/output")).removesuffix(":/output"))
(out / "rigcoder").write_bytes(b"synthetic binary")
attack = os.environ.get("BUILD_ATTACK")
canary = pathlib.Path(os.environ["ORIGINAL_SOURCE"]).parents[3] / "harness/holdout-canary"
if attack == "receipt_link":
    (out / "receipt.json").symlink_to(canary)
elif attack:
    binary = out / "rigcoder"
    binary.unlink()
    if attack == "binary_link": binary.symlink_to(canary)
    elif attack == "hardlink": os.link(canary, binary)
    elif attack == "fifo": os.mkfifo(binary)
    elif attack == "oversized":
        with binary.open("wb") as output: output.truncate(512 * 1024 * 1024 + 1)
''')
        docker.chmod(0o755)

    def build(self, **extra):
        env = dict(os.environ, PATH=f"{self.root / 'fakebin'}:{os.environ['PATH']}", ARCH="aarch64", ORIGINAL_SOURCE=str(self.source))
        env.update(extra)
        return subprocess.run(["bash", "harness/build-linux.sh"], cwd=self.root, env=env, capture_output=True, text=True)

    def test_candidate_output_cannot_redirect_host_publication_or_cleanup(self):
        canary = self.root / "harness/holdout-canary"
        canary.chmod(0o400)
        for attack in ("receipt_link", "binary_link", "hardlink", "fifo", "oversized"):
            with self.subTest(attack=attack):
                self.source.write_text("captured source\n")
                result = self.build(BUILD_ATTACK=attack)
                self.assertEqual(result.returncode == 0, attack == "receipt_link", result.stderr)
                self.assertEqual(canary.read_text(), "hidden evaluation input")
                self.assertEqual(canary.stat().st_mode & 0o777, 0o400)
                binary = self.root / "harness/bin/rigcoder-linux-aarch64"
                if attack == "receipt_link":
                    self.assertEqual(binary.read_bytes(), b"synthetic binary")
                else:
                    self.assertFalse(binary.exists())
                    self.assertFalse(binary.with_name(binary.name + ".build.json").exists())

    def test_receipt_identifies_captured_source_and_binary_without_hidden_inputs(self):
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        receipt = json.loads((self.root / "harness/bin/rigcoder-linux-aarch64.build.json").read_text())
        self.assertEqual(receipt["inputs"]["crates/cli/src/main.rs"]["sha256"], hashlib.sha256(b"captured source\n").hexdigest())
        self.assertEqual(receipt["binary_sha256"], hashlib.sha256(b"synthetic binary").hexdigest())
        self.assertEqual(receipt["builder_image"], "sha256:" + "a" * 64)
        self.assertFalse(any(name.startswith(("harness/", ".git/")) for name in receipt["inputs"]))
        self.assertEqual(self.source.read_text(), "changed after capture\n")

    def test_source_link_cannot_import_hidden_input(self):
        self.source.unlink()
        self.source.symlink_to(self.root / "harness/holdout-canary")
        result = self.build()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("regular files/directories", result.stderr)
        self.assertFalse((self.root / "harness/bin/rigcoder-linux-aarch64").exists())

    def test_builder_platform_must_match_requested_architecture(self):
        docker = self.root / "fakebin/docker"
        docker.write_text(docker.read_text().replace("linux/arm64", "linux/amd64"))
        result = self.build()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("builder image platform mismatch", result.stderr)
        self.assertFalse((self.root / "harness/bin/rigcoder-linux-aarch64").exists())

    def test_receipt_validation_rejects_stale_binary_source_and_settings(self):
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.source.write_text("captured source\n")
        binary = self.root / "harness/bin/rigcoder-linux-aarch64"
        receipt = binary.with_name(binary.name + ".build.json")

        def verify(arch="aarch64"):
            return subprocess.run(["python3", "harness/build-inputs.py", "verify", str(self.root), str(binary), str(receipt), arch], cwd=self.root, capture_output=True)

        self.assertEqual(verify().returncode, 0)
        original_receipt = receipt.read_text()
        invalid = json.loads(original_receipt)
        invalid["builder_image"] = "rust:latest"
        receipt.write_text(json.dumps(invalid))
        self.assertNotEqual(verify().returncode, 0)
        receipt.write_text(original_receipt)
        self.assertNotEqual(verify("x86_64").returncode, 0)
        for path in (binary, self.source, self.root / "Cargo.lock"):
            with self.subTest(path=path.name):
                original = path.read_bytes()
                path.write_bytes(original + b"changed")
                self.assertNotEqual(verify().returncode, 0)
                path.write_bytes(original)
        extra = self.source.with_name("new.rs")
        extra.write_text("new input")
        self.assertNotEqual(verify().returncode, 0)
        extra.unlink()
        mode = self.source.stat().st_mode
        self.source.chmod(0o600)
        self.assertNotEqual(verify().returncode, 0)
        self.source.chmod(mode)
        self.assertEqual(verify().returncode, 0)

    def test_read_only_input_directory_does_not_break_cleanup(self):
        directory = self.root / "crates/read-only"
        directory.mkdir()
        (directory / "input").write_text("read-only source")
        directory.chmod(0o555)
        try:
            result = self.build()
            self.assertEqual(result.returncode, 0, result.stderr)
        finally:
            directory.chmod(0o755)

    def test_failed_build_or_receipt_publication_removes_both_outputs(self):
        for failure in ("docker", "receipt"):
            with self.subTest(failure=failure):
                output = self.root / "harness/bin/rigcoder-linux-aarch64"
                receipt = output.with_name(output.name + ".build.json")
                output.parent.mkdir(exist_ok=True)
                output.write_text("old binary")
                receipt.write_text("old receipt")
                fake = self.root / "fakebin" / ("docker" if failure == "docker" else "mv")
                before = fake.read_text() if fake.exists() else None
                fake.write_text("#!/bin/sh\nexit 1\n" if failure == "docker" else "#!/bin/sh\ncase \"$2\" in *.build.json) exit 1 ;; esac\nexec /bin/mv \"$@\"\n")
                fake.chmod(0o755)
                result = self.build()
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(output.exists())
                self.assertFalse(receipt.exists())
                if before is None:
                    fake.unlink()
                else:
                    fake.write_text(before)


if __name__ == "__main__":
    unittest.main()
