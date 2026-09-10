"""Exercise the real build script with adversarial, dependency-free Rust inputs."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def check():
    harness = Path(__file__).resolve().parent
    with tempfile.TemporaryDirectory(prefix="rigcoder-build-canary-") as directory:
        root = Path(directory)
        (root / "harness").mkdir()
        for name in ("build-linux.sh", "build-inputs.py"):
            shutil.copyfile(harness / name, root / "harness" / name)
        (root / "harness/secret").write_text("SYNTHETIC_SEALED_VALUE")
        (root / "harness/secret").chmod(0o400)
        (root / "Cargo.toml").write_text('[workspace]\nmembers=["crates/cli"]\nresolver="2"\n')
        (root / "Cargo.lock").write_text('version = 4\n[[package]]\nname="rigcoder-cli"\nversion="0.1.0"\n')
        (root / "rust-toolchain.toml").write_text('[toolchain]\nchannel="1.95.0"\n')
        crate = root / "crates/cli"
        (crate / "src").mkdir(parents=True)
        (crate / "Cargo.toml").write_text('[package]\nname="rigcoder-cli"\nversion="0.1.0"\nedition="2024"\n[[bin]]\nname="rigcoder"\npath="src/main.rs"\n')
        (crate / "src/main.rs").write_text('fn main() { println!("synthetic build"); }')
        (crate / "build.rs").write_text(r'''
use std::{fs, path::Path};
fn main() {
    assert!(!Path::new("/src/harness/secret").exists(), "hidden harness mounted");
    assert!(!Path::new("/src/.git").exists(), "Git history mounted");
    assert!(std::env::var("BUILD_CANARY_SECRET").is_err(), "host secret inherited");
    assert!(fs::write("/cargo/poison", "candidate state").is_err(), "dependency cache writable");
    assert!(fs::write("/src/Cargo.toml", "candidate state").is_err(), "source writable");
    assert!(!Path::new("/build/poison").exists(), "previous target state reused");
    fs::write("/build/poison", "candidate state").unwrap();
    std::os::unix::fs::symlink(HOST_CANARY, "/output/receipt.json").unwrap();
    let interfaces: Vec<_> = fs::read_dir("/sys/class/net").unwrap()
        .map(|entry| entry.unwrap().file_name()).collect();
    assert_eq!(interfaces, vec![std::ffi::OsString::from("lo")], "external network interface present");
}
'''.replace("HOST_CANARY", json.dumps(str(root / "harness/secret"))))
        subprocess.run(["git", "init", "-q", root], check=True)
        subprocess.run(["git", "-C", root, "add", "."], check=True)
        subprocess.run(["git", "-C", root, "-c", "user.name=Build canary", "-c",
                        "user.email=canary@example.invalid", "commit", "-qm", "synthetic baseline"], check=True)
        environment = {**os.environ, "BUILD_CANARY_SECRET": "SYNTHETIC_SEALED_VALUE"}
        # Two builds prove that candidate-written target state is not reused.
        for _ in range(2):
            subprocess.run(["bash", "harness/build-linux.sh"], cwd=root,
                           env=environment, check=True, timeout=900)
            receipts = list((root / "harness/bin").glob("*.build.json"))
            assert len(receipts) == 1
            receipt = json.loads(receipts[0].read_text())
            assert receipt["builder_image"].startswith("sha256:")
            assert not any(name.startswith("harness/") for name in receipt["inputs"])
            assert (root / "harness/secret").read_text() == "SYNTHETIC_SEALED_VALUE"
            assert (root / "harness/secret").stat().st_mode & 0o777 == 0o400
    print("PASS: real compilation denies hidden inputs, network and cache mutation; target state is fresh")


if __name__ == "__main__":
    check()
