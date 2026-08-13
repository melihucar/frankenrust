#!/usr/bin/env bash
# Runs an arbitrary command inside the frankenrust-dev image (PHP 8.5
# ZTS+embed, plus a pinned Rust toolchain and libclang) with the repo
# bind-mounted at /work.
#
#   scripts/dev.sh cargo build --workspace --all-targets
#   scripts/dev.sh php-config --php-sapis
#
# The host this normally runs on has no PHP embed SAPI and no Rust toolchain
# (see docker/frankenrust-dev.Dockerfile), so every command that needs to
# build or link against libphp goes through here. scripts/gate.sh routes its
# build/fmt/clippy/test steps through this script for exactly that reason.
#
# Docker being unavailable, or the image failing to build, must FAIL this
# script (nonzero exit) rather than silently skip — the gate turns that
# failure into a failed step, not a green one.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

IMAGE="frankenrust-dev"
DOCKERFILE="docker/frankenrust-dev.Dockerfile"
PLATFORM="linux/arm64"
# Named volumes, not bind mounts: target/ and the cargo registry churn on
# every build, and Docker Desktop on macOS serves bind mounts over VirtioFS —
# bench/harness/run.sh:70-74 measured a 6.6x artifact from exactly that for a
# server's document root. A named volume keeps both off the VirtioFS path and
# off the host filesystem entirely, so Linux build artifacts never land in a
# host target/ a native toolchain might also use.
CARGO_REGISTRY_VOLUME="frankenrust-cargo-registry"
TARGET_VOLUME="frankenrust-target"

if [ $# -eq 0 ]; then
  echo "usage: $0 <command> [args...]" >&2
  exit 2
fi

command -v docker >/dev/null 2>&1 || {
  echo "dev.sh: docker is not installed or not on PATH" >&2
  exit 1
}
docker info >/dev/null 2>&1 || {
  echo "dev.sh: docker daemon is not reachable (is Docker running?)" >&2
  exit 1
}

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "dev.sh: building $IMAGE (one time; cached after this)" >&2
  docker build --platform "$PLATFORM" -t "$IMAGE" -f "$DOCKERFILE" . || {
    echo "dev.sh: image build failed" >&2
    exit 1
  }
fi

docker volume inspect "$CARGO_REGISTRY_VOLUME" >/dev/null 2>&1 \
  || docker volume create "$CARGO_REGISTRY_VOLUME" >/dev/null \
  || { echo "dev.sh: could not create volume $CARGO_REGISTRY_VOLUME" >&2; exit 1; }
docker volume inspect "$TARGET_VOLUME" >/dev/null 2>&1 \
  || docker volume create "$TARGET_VOLUME" >/dev/null \
  || { echo "dev.sh: could not create volume $TARGET_VOLUME" >&2; exit 1; }

TTY_FLAGS=(-i)
[ -t 1 ] && TTY_FLAGS+=(-t)

exec docker run --rm "${TTY_FLAGS[@]}" \
  --platform "$PLATFORM" \
  -v "$PWD":/work \
  -v "$TARGET_VOLUME":/work/target \
  -v "$CARGO_REGISTRY_VOLUME":/usr/local/cargo/registry \
  -w /work \
  "$IMAGE" "$@"
