#!/usr/bin/env bash
# Build the Linux rigcoder binary the Harbor adapter uploads into task
# containers. Uses Docker so the build matches the container's libc, with
# fresh per-build dependency and target directories. Compilation has no network.
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
BUILD_CONTAINER="rigcoder-build-${BUILD_INPUTS_DIR##*/}"
cleanup() {
  build_status=$?
  docker rm -f "$BUILD_CONTAINER" >/dev/null 2>&1 || true
  if [ "$build_status" -ne 0 ]; then
    rm -f "$BUILD_BINARY" "$BUILD_RECEIPT" || true
  fi
  # Captured source directories may be read-only. Cleanup must not skip
  # output invalidation or replace an earlier failure through errexit.
  if ! python3 harness/build-inputs.py cleanup "$BUILD_INPUTS_DIR"; then
    rm -f "$BUILD_BINARY" "$BUILD_RECEIPT" || true
    if [ "$build_status" -eq 0 ]; then build_status=1; fi
  fi
  return "$build_status"
}
trap cleanup EXIT
python3 harness/build-inputs.py snapshot "$PWD" "$BUILD_INPUTS_DIR/src" > "$BUILD_INPUTS_DIR/source.json"
mkdir "$BUILD_INPUTS_DIR/output" "$BUILD_INPUTS_DIR/cargo" "$BUILD_INPUTS_DIR/target"
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
# Install trusted system dependencies without exposing source or evaluation data.
# Docker may cache this image layer; no candidate-generated state enters it.
docker build --platform "$PLATFORM" --iidfile "$BUILD_INPUTS_DIR/builder.id" - <<EOF
FROM $BUILD_IMAGE
RUN apt-get update -qq && apt-get install -y -qq pkg-config >/dev/null && rm -rf /var/lib/apt/lists/*
EOF
BUILD_IMAGE="$(cat "$BUILD_INPUTS_DIR/builder.id")"
if [[ ! "$BUILD_IMAGE" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "invalid prepared builder image ID" >&2
  exit 1
fi
BUILD_USER="$(id -u):$(id -g)"
# Fetch only: Cargo does not execute build scripts during dependency fetching.
# These manifests are trusted baseline inputs; improvement lanes cannot edit them.
docker run --rm --name "$BUILD_CONTAINER" --platform "$PLATFORM" --user "$BUILD_USER" \
  --read-only --tmpfs /tmp:rw,nosuid,nodev --cap-drop ALL \
  --security-opt no-new-privileges --pids-limit 512 --memory 8g --cpus 2 \
  -v "$BUILD_INPUTS_DIR/src":/src:ro -w /src \
  -v "$BUILD_INPUTS_DIR/cargo":/cargo \
  -e CARGO_HOME=/cargo -e HOME=/tmp \
  "$BUILD_IMAGE" cargo fetch --locked
# Fresh dependency state is read-only during compilation. No persistent target
# volume, Git history, repository harness, credentials or hidden tasks are mounted.
docker run --rm --name "$BUILD_CONTAINER" --platform "$PLATFORM" --user "$BUILD_USER" --network none \
  --read-only --tmpfs /tmp:rw,nosuid,nodev --cap-drop ALL \
  --security-opt no-new-privileges --pids-limit 512 --memory 8g --cpus 2 \
  -v "$BUILD_INPUTS_DIR/src":/src:ro -w /src \
  -v "$BUILD_INPUTS_DIR/cargo":/cargo:ro \
  -v "$BUILD_INPUTS_DIR/target":/build \
  -v "$BUILD_INPUTS_DIR/output":/output \
  -e CARGO_HOME=/cargo -e HOME=/tmp -e CARGO_TARGET_DIR=/build -e CARGO_NET_OFFLINE=true \
  "$BUILD_IMAGE" \
  bash -c 'cargo build --locked --release -p rigcoder-cli && cp /build/release/rigcoder /output/rigcoder'
python3 harness/build-inputs.py capture "$BUILD_INPUTS_DIR/output/rigcoder" "$BUILD_INPUTS_DIR/rigcoder"
python3 harness/build-inputs.py receipt "$BUILD_INPUTS_DIR/source.json" "$BUILD_INPUTS_DIR/rigcoder" "$ARCH" "$BUILD_IMAGE" > "$BUILD_INPUTS_DIR/receipt.json"
mv "$BUILD_INPUTS_DIR/rigcoder" "$BUILD_BINARY"
mv "$BUILD_INPUTS_DIR/receipt.json" "$BUILD_RECEIPT"
echo "built harness/bin/rigcoder-linux-$ARCH"
