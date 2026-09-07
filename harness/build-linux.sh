#!/usr/bin/env bash
# Build the Linux rigcoder binary the Harbor adapter uploads into task
# containers. Uses Docker so the build matches the container's libc, with
# the cargo registry and target dir cached in named volumes.
#
#   ./harness/build-linux.sh            # aarch64 on Apple Silicon, x86_64 elsewhere
#   ARCH=x86_64 ./harness/build-linux.sh
set -euo pipefail
cd "$(dirname "$0")/.."
ARCH="${ARCH:-$( [ "$(uname -m)" = arm64 ] && echo aarch64 || echo x86_64 )}"
PLATFORM="linux/$( [ "$ARCH" = aarch64 ] && echo arm64 || echo amd64 )"
RUST="${RUST_VERSION:-1.95.0}"
mkdir -p harness/bin
docker run --rm --platform "$PLATFORM" \
  -v "$PWD":/src -w /src \
  -v "rigcoder-cargo-registry-$ARCH":/usr/local/cargo/registry \
  -v "rigcoder-cargo-git-$ARCH":/usr/local/cargo/git \
  -v "rigcoder-target-$ARCH":/src/target-linux \
  -e CARGO_TARGET_DIR=/src/target-linux \
  "rust:$RUST-bookworm" \
  bash -c 'apt-get update -qq && apt-get install -y -qq pkg-config >/dev/null && cargo build --release -p rigcoder-cli && cp target-linux/release/rigcoder /src/harness/bin/rigcoder-linux-'"$ARCH"
echo "built harness/bin/rigcoder-linux-$ARCH"
