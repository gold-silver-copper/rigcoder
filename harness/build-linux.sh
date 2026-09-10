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
case "$ARCH" in aarch64|x86_64) ;; *) echo "unsupported architecture: $ARCH" >&2; exit 1 ;; esac
PLATFORM="linux/$( [ "$ARCH" = aarch64 ] && echo arm64 || echo amd64 )"
RUST="${RUST_VERSION:-1.95.0}"
mkdir -p harness/bin
BUILD_BINARY="harness/bin/rigcoder-linux-$ARCH"
BUILD_RECEIPT="$BUILD_BINARY.build.json"
rm -f "$BUILD_BINARY" "$BUILD_RECEIPT"
BUILD_INPUTS_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rigcoder-build.XXXXXXXX")"
cleanup() {
  build_status=$?
  if [ "$build_status" -ne 0 ]; then
    rm -f "$BUILD_BINARY" "$BUILD_RECEIPT" || true
  fi
  # Captured source directories may be read-only. Cleanup must not skip
  # output invalidation or replace an earlier failure through errexit.
  chmod -R u+rwX "$BUILD_INPUTS_DIR" || true
  if ! rm -rf "$BUILD_INPUTS_DIR"; then
    rm -f "$BUILD_BINARY" "$BUILD_RECEIPT" || true
    if [ "$build_status" -eq 0 ]; then build_status=1; fi
  fi
  return "$build_status"
}
trap cleanup EXIT
python3 harness/build-inputs.py snapshot "$PWD" "$BUILD_INPUTS_DIR/src" > "$BUILD_INPUTS_DIR/source.json"
mkdir "$BUILD_INPUTS_DIR/output"
docker pull --platform "$PLATFORM" "rust:$RUST-bookworm"
BUILD_IMAGE_DETAILS="$(docker image inspect --format '{{.Id}} {{.Os}}/{{.Architecture}}' "rust:$RUST-bookworm")"
read -r BUILD_IMAGE BUILD_PLATFORM <<< "$BUILD_IMAGE_DETAILS"
if [[ ! "$BUILD_IMAGE" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "invalid builder image ID" >&2
  exit 1
fi
if [ "$BUILD_PLATFORM" != "$PLATFORM" ]; then
  echo "builder image platform mismatch: $BUILD_PLATFORM, expected $PLATFORM" >&2
  exit 1
fi
docker run --rm --platform "$PLATFORM" \
  -v "$BUILD_INPUTS_DIR/src":/src:ro -w /src \
  -v "$BUILD_INPUTS_DIR/output":/output \
  -v "rigcoder-cargo-registry-$ARCH":/usr/local/cargo/registry \
  -v "rigcoder-cargo-git-$ARCH":/usr/local/cargo/git \
  -v "rigcoder-target-$ARCH":/build \
  -e CARGO_TARGET_DIR=/build \
  "$BUILD_IMAGE" \
  bash -c 'apt-get update -qq && apt-get install -y -qq pkg-config >/dev/null && cargo build --locked --release -p rigcoder-cli && cp /build/release/rigcoder /output/rigcoder'
python3 harness/build-inputs.py receipt "$BUILD_INPUTS_DIR/source.json" "$BUILD_INPUTS_DIR/output/rigcoder" "$ARCH" "$BUILD_IMAGE" > "$BUILD_INPUTS_DIR/output/receipt.json"
mv "$BUILD_INPUTS_DIR/output/rigcoder" "$BUILD_BINARY"
mv "$BUILD_INPUTS_DIR/output/receipt.json" "$BUILD_RECEIPT"
echo "built harness/bin/rigcoder-linux-$ARCH"
