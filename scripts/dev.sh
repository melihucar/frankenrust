#!/usr/bin/env bash
# Runs an arbitrary command inside the frankenrust-dev image (see
# docker/frankenrust-dev.Dockerfile), with the repo bind-mounted at /work.
#
#   scripts/dev.sh cargo build --workspace --all-targets
#   scripts/dev.sh php-config --php-sapis
#   scripts/dev.sh php -r 'echo PHP_ZTS;'
#
# This is the only place that can build Rust against libphp: the host has no
# Rust toolchain and Homebrew's PHP is neither ZTS nor built with the embed
# SAPI (see docker/frankenrust-dev.Dockerfile and issue #5). scripts/gate.sh
# routes its build/fmt/clippy/test steps through this script; it must FAIL,
# not silently pass or skip, if Docker is unavailable or the image cannot be
# built — a gate step that quietly no-ops is worse than no gate at all.
#
# target/ and cargo's home (the registry included) live on named volumes, not
# bind mounts.
# Docker Desktop on macOS serves bind mounts over VirtioFS, and a bind-mounted
# target/ pays a large, measured tax for it (bench/harness/run.sh:70-74: 6.6x
# for the equivalent case with a benchmark fixture). A named volume also
# keeps Linux-built artifacts out of the host's target/, which the host
# cannot use anyway since it has no toolchain to link them.
#
# Both names below are derived, not constant, and tests/dev-env/dev-sh.test.sh
# pins that. See the comments at each derivation for what goes wrong otherwise.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

DOCKERFILE="docker/frankenrust-dev.Dockerfile"
WORKTREE="$(pwd -P)"

# Every target/ volume carries the absolute path of the worktree it belongs to,
# so a later run can tell which volumes outlived their worktree. See reclaim
# below.
WORKTREE_LABEL="com.frankenrust.worktree"

# ...and the image it was last built against. target/ holds cargo's build
# cache, which is only valid for the libphp headers/ABI baked into that image
# — and that dependency is invisible to cargo: the image content lives outside
# the bind-mounted /work tree, so no source-file mtime or content hash cargo
# tracks will ever change when PHP_DIGEST is bumped. Without this, a target/
# volume built against a stale image survives a Dockerfile edit (e.g. #7's
# bindgen picking up a new libphp ABI) and gets silently relinked against the
# new one instead of rebuilt. See the invalidation check below.
IMAGE_LABEL="com.frankenrust.image"

if [ $# -eq 0 ]; then
  echo "usage: $0 <command> [args...]" >&2
  exit 2
fi

# First 12 hex of the sha256 of stdin. No weaker fallback on purpose: both
# names below are derived from this, and a collision silently reintroduces the
# cross-worktree artifact reuse that the per-worktree volume exists to prevent.
# A machine with none of these three is a machine we would rather stop on.
sha12() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 -r
  else
    echo "FATAL: need shasum, sha256sum or openssl to derive image and volume names" >&2
    return 1
  fi | cut -c1-12
}

# Tag the image with a digest of the Dockerfile that produced it. Tagging a
# constant (":latest") and rebuilding only when it is absent means the first
# build on a host wins forever: every later edit to the Dockerfile — a new
# rustup component, a bumped PHP_DIGEST — is silently ignored, and the gate's
# behaviour is defined by a local artifact that disagrees with the repo. With
# the digest in the tag, an edited Dockerfile is a cache miss by construction,
# and BuildKit's layer cache still makes the unchanged prefix free.
DOCKERFILE_HASH="$(sha12 < "$DOCKERFILE")" || exit 1
[ -n "$DOCKERFILE_HASH" ] || { echo "FATAL: could not hash $DOCKERFILE" >&2; exit 1; }
IMAGE="${FRANKENRUST_DEV_IMAGE:-frankenrust-dev:$DOCKERFILE_HASH}"

# One target/ volume per worktree. A shared volume is not merely a contention
# problem: the orchestrator runs up to FR_PARALLEL worktrees at once
# (orchestrator/loop.py:62) and every one of them mounts the repo at the same
# container path, /work. Cargo's unit fingerprints are keyed on the package id
# and that path, so units from different worktrees collide onto identical
# fingerprint files and identical output filenames; freshness then degrades to
# an mtime comparison, and a worktree whose sources predate artifacts another
# worktree just wrote is declared Fresh. The gate then reports build, clippy
# and test green for code that was never compiled, and runs the other
# worktree's test binaries. Reproduced by two reviewers on issue #5.
WORKTREE_HASH="$(printf '%s' "$WORKTREE" | sha12)" || exit 1
[ -n "$WORKTREE_HASH" ] || { echo "FATAL: could not hash $WORKTREE" >&2; exit 1; }

# ...and one per image on top of that, because the image is the other half of
# what the cache is valid for (see IMAGE_LABEL above). The image belongs in the
# NAME rather than in a check that discards a wrongly-labelled volume in place:
# a name is derived state, and deriving it needs no permission from anyone.
# Removing a shared volume needs the daemon's, and the daemon says no whenever
# any container still references it — including one that has already exited but
# not yet been reaped. Both of these are exit 1 today:
#
#   $ docker volume rm V   # peer container running, or merely exited
#   Error response from daemon: remove V: volume is in use - [fc13f294f284]
#   $ docker volume rm V   # ...or a peer got there first
#   Error response from daemon: get V: no such volume
#
# and step() in scripts/gate.sh:20 turns either into `FAIL build` for a
# worktree whose code is fine. That is not hypothetical: the loop runs
# FR_PARALLEL gates at once (orchestrator/loop.py:62 -> 3), so any merge that
# edits this Dockerfile — a PHP_DIGEST bump, #7 adding a build dependency —
# lands while peers hold the old volume and costs an innocent agent an attempt
# (loop.py:538-543). Caught by a reviewer on #5, who reproduced exactly that.
#
# Hashed rather than interpolated raw: $IMAGE carries a ':' (and, if
# FRANKENRUST_DEV_IMAGE names a registry, '/'), neither of which is legal in a
# volume name. Hashing $IMAGE and not $DOCKERFILE_HASH is deliberate — with the
# override set, the Dockerfile no longer says anything about which image runs.
IMAGE_HASH="$(printf '%s' "$IMAGE" | sha12)" || exit 1
[ -n "$IMAGE_HASH" ] || { echo "FATAL: could not hash $IMAGE" >&2; exit 1; }

# Every volume this worktree has ever owned shares this prefix; exactly one of
# them is current. The reclaim loop below uses that to retire the rest.
TARGET_VOLUME_PREFIX="frankenrust-dev-target-$WORKTREE_HASH"
TARGET_VOLUME="$TARGET_VOLUME_PREFIX-$IMAGE_HASH"

# The registry stays shared: it is a content-addressed download cache, and
# re-downloading it per worktree is pure waste. But sharing it safely means
# sharing the WHOLE of CARGO_HOME, not just registry/. Cargo's package-cache
# lock — the mutex that serialises unpacking a .crate into registry/src — is
# $CARGO_HOME/.package-cache, one level ABOVE registry/ (likewise
# .package-cache-mutate and .global-cache). Mount only registry/ and every
# container locks a file on its own writable layer, i.e. takes no lock at all,
# while all of them mutate one shared registry/src.
#
# That is the default configuration, not an exotic one: orchestrator/loop.py:679
# runs FR_PARALLEL (loop.py:62 -> 3) gates concurrently and each one comes
# through here. Reproduced on a cold registry, three concurrent `cargo check`s
# against one shared volume: one died with "failed to unpack package `socket2
# v0.6.5` ... .cargo-ok: File exists (os error 17)"; the same three sharing all
# of CARGO_HOME finished 3/3 clean. A failure there reads as `FAIL build` and
# costs an innocent agent an attempt (loop.py:538-543).
#
# CARGO_HOME is therefore relocated to a path that IS the mount point. It
# cannot be the image's /usr/local/cargo: a named volume mounted over a
# populated image directory is seeded from that directory once and then pins
# it, so a Rust version bump in the Dockerfile would be shadowed forever by
# the toolchain copy the volume made on some earlier host.
CARGO_HOME_IN_CONTAINER="/cargo"
CARGO_HOME_VOLUME="frankenrust-dev-cargo-home"

if ! docker info >/dev/null 2>&1; then
  echo "FATAL: docker is unavailable (daemon unreachable)" >&2
  exit 1
fi

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "-- $IMAGE not found locally, building it" >&2
  # Dockerfile on stdin, so the build context is empty. Nothing in the
  # Dockerfile COPYs from the context, and the repo root is a bad context to
  # ship to the daemon: run from the orchestrator's own checkout it contains
  # every agent worktree and their target/ dirs.
  docker build --platform linux/arm64 -t "$IMAGE" - < "$DOCKERFILE" || {
    echo "FATAL: failed to build $IMAGE" >&2
    exit 1
  }

  # A tag change is a cache miss by construction (see DOCKERFILE_HASH above),
  # so every edit to the Dockerfile mints a new tag and nothing ever untagged
  # the old one — on a long-running host that is one multi-hundred-MB image
  # accumulating per edit, forever. Only prune right after a build, not on
  # every invocation: the set of tags can't have changed in between.
  docker images --format '{{.Repository}}:{{.Tag}}' "${IMAGE%%:*}" 2>/dev/null |
    while read -r old; do
      [ -n "$old" ] && [ "$old" != "$IMAGE" ] || continue
      docker rmi "$old" >/dev/null 2>&1 &&
        echo "-- removed superseded image $old" >&2
    done
fi

# Per-worktree, per-image volumes would otherwise leak one target/ per issue
# and per image across a long run, and for a workspace that bindgens PHP those
# are gigabytes each (orchestrator/loop.py:366-372 makes the same point about
# worktrees). Reclaim only volumes this script created and labelled — the loop
# deliberately never prunes a user's docker resources, and neither does this.
# The --format string repeats the label literally because Go template syntax
# and shell quoting do not mix legibly; keep it in step with $WORKTREE_LABEL.
#
# Two things are reclaimable, and the second is why putting the image in the
# volume name does not simply trade a race for a disk leak:
#
#   1. the worktree it belongs to is gone;
#   2. it belongs to THIS worktree but an older image — the case the old
#      remove-then-recreate check handled, minus the hard failure.
#
# Case 2 is restricted to our own worktree on purpose. A sibling worktree on a
# different image is not stale, it is a peer mid-run with a different branch
# checked out, and its cache is none of our business.
#
# Caveat, deliberate: while #29 is open every gate chdirs to the orchestrator's
# own checkout, so concurrent gates on *different* Dockerfiles resolve the same
# $WORKTREE and read as case 2 to each other — they will retire each other's
# caches and re-download. That is a slow gate, not a failing one, and it is
# strictly better than what this replaced (the same collision was a hard
# exit 1). It disappears when #29 lands and worktrees stop aliasing.
#
# Both are best-effort by construction: a reclaim that cannot happen is a leak
# to warn about, never a reason to fail a gate. That is the whole point of the
# rename — correctness no longer depends on a removal succeeding.
docker volume ls --filter "label=$WORKTREE_LABEL" \
  --format '{{.Name}} {{.Label "com.frankenrust.worktree"}}' 2>/dev/null |
  while read -r vol path; do
    [ -n "$vol" ] && [ -n "$path" ] || continue
    # The volume we are about to mount. Never a reclaim candidate.
    [ "$vol" = "$TARGET_VOLUME" ] && continue

    if [ ! -d "$path" ]; then
      reason="worktree $path is gone"
    else
      # Ours, but not our current image. The bare prefix with no suffix is an
      # old-scheme name from before the image was part of it; it is equally
      # superseded. No other worktree can match this pattern — the prefix ends
      # in the full hash of our own path.
      case "$vol" in
        "$TARGET_VOLUME_PREFIX"|"$TARGET_VOLUME_PREFIX"-*)
          reason="superseded by $IMAGE" ;;
        *) continue ;;
      esac
    fi

    # </dev/null: without it, this call inherits stdin from the `docker
    # volume ls | while read` pipe above it. bash does not hand `read` the
    # pipe one line at a time — it can buffer ahead — so a command run inside
    # the loop that shares that same fd can silently consume bytes `read`
    # was still going to need, and every candidate after the one being
    # processed vanishes from the loop with no error from anything. Verified
    # by reproducing it directly: two dead-worktree volumes queued for
    # reclaim, only the first one ever got a `volume rm`, and nothing here
    # -- no test, no exit code -- said so.
    if docker volume rm "$vol" >/dev/null 2>&1 </dev/null; then
      echo "-- reclaimed $vol ($reason)" >&2
    else
      # Silent failure here means the leak this loop exists to prevent
      # (target/ volumes run to gigabytes) accumulates with nothing to show
      # for it — most likely cause is the volume is still attached to a
      # container (e.g. one orphaned by a killed gate run, or a peer gate
      # still using it).
      echo "-- WARNING: could not reclaim $vol ($reason); still in use?" >&2
    fi
  done

# The scan above finds volumes BY label, so it can never see one that reached
# the daemon without one. That happens: `docker volume create` is checked
# below, but `docker volume create NAME` issued twice for the same not-yet-
# existing NAME is not an error on the second caller — the daemon silently
# hands back the existing volume instead of failing — so two dev.sh
# invocations racing on this exact name (concurrent gates against the same
# worktree+image, or a peer manually probing it) can have the loser believe it
# created something it only reused, and there is no fixed number of retries
# that rules out an even earlier, unlabelled write to that name from before
# this labelling scheme existed. A reviewer on #5 reproduced exactly this:
# found a live orphan (`docker volume inspect`: `Labels: map[]`) whose name
# traced to a superseded image hash, and confirmed the general mechanism with
# `docker run --rm -v probe:/x alpine true` — an unlabelled volume that
# `docker volume ls --filter label=...` will never return again.
#
# The fix is to stop trusting existence and start trusting the name: every
# volume this worktree has ever owned matches $TARGET_VOLUME_PREFIX (its hash
# comes from $WORKTREE and nothing else could have produced it), so ANY such
# volume that is not properly labelled — $TARGET_VOLUME itself included — is
# not one we can vouch for and is reclaimed unconditionally. A labelled one is
# left alone here; it is either the volume this run is about to reuse, or one
# the scan above has already judged (or will next run).
docker volume ls --filter "name=$TARGET_VOLUME_PREFIX" --format '{{.Name}}' 2>/dev/null |
  while read -r vol; do
    [ -n "$vol" ] || continue
    # </dev/null on both calls below: see the identical comment on the
    # labelled scan above -- either one left to inherit the pipe's stdin can
    # make `read` silently drop every candidate after the one being checked.
    label="$(docker volume inspect -f "{{index .Labels \"$WORKTREE_LABEL\"}}" "$vol" 2>/dev/null </dev/null)"
    [ -n "$label" ] && continue
    if docker volume rm "$vol" >/dev/null 2>&1 </dev/null; then
      echo "-- reclaimed $vol (unlabelled, but named for this worktree/image)" >&2
    else
      echo "-- WARNING: could not reclaim unlabelled $vol; still in use?" >&2
    fi
  done

# $TARGET_VOLUME is by construction a cache built against $IMAGE and nothing
# else: a different image is a different name, so there is no such thing as
# reusing a stale one and no check here to get wrong. Reusing it when it does
# exist is the entire point — that is the warm cache. And thanks to the
# unlabelled-reclaim pass just above, "does exist" now implies "is labelled":
# an existing-but-unlabelled $TARGET_VOLUME was already removed there, so
# reaching this line and finding it present means it is ours.
#
# The image label is no longer load-bearing (the name carries that now) and is
# kept for legibility: `docker volume ls` shows a readable tag where the name
# has only a hash of it.
#
# Checked, unlike most `docker volume create` calls one sees: `docker run -v`
# auto-creates a missing named volume, so a failure here does not surface as a
# failed run — it surfaces as a volume with the right name and NO labels. The
# unlabelled-reclaim pass above will catch that on the NEXT invocation (it
# scans by name, not by label), but this run must not silently hand the
# caller a volume nothing has vouched for, so it still fails loudly here.
docker volume inspect "$TARGET_VOLUME" >/dev/null 2>&1 ||
  docker volume create --label "$WORKTREE_LABEL=$WORKTREE" --label "$IMAGE_LABEL=$IMAGE" \
    "$TARGET_VOLUME" >/dev/null || {
    echo "FATAL: could not create $TARGET_VOLUME" >&2
    exit 1
  }
# Deliberately not checked, unlike the target volume above: this one is a
# singleton under a constant name and carries no labels, so the volume `docker
# run -v` would auto-create in its place is identical to the one we asked for.
# There is no label to lose and so nothing for a silent failure to leak.
if ! docker volume inspect "$CARGO_HOME_VOLUME" >/dev/null 2>&1; then
  docker volume create "$CARGO_HOME_VOLUME" >/dev/null
  # An earlier dev.sh shared only $CARGO_HOME/registry, under its own volume
  # name (see above for why that was wrong). Nothing mounts that volume any
  # more, it carries none of the labels the reclaim loop filters on, and a
  # populated cargo registry runs to hundreds of MB — so reclaim it here, on
  # the one run that mints its replacement. Still-in-use (an older worktree
  # mid-run) makes `rm` fail, which is fine: this is a leak, not a hazard.
  docker volume rm frankenrust-dev-cargo-registry >/dev/null 2>&1 &&
    echo "-- reclaimed frankenrust-dev-cargo-registry (superseded by $CARGO_HOME_VOLUME)" >&2
fi

# Mounted at /target, NOT /work/target: a named volume nested under a bind
# mount is incoherent under Docker Desktop's overlayfs driver (verified
# 2026-08-18, Docker Desktop 29.7.2): cargo writes fingerprints and build
# output that vanish mid-build -- "failed to load metadata for path
# .../invoked.timestamp: No such file or directory" -- while the same volume
# mounted at a top-level path is fully coherent in the same container, same
# image, same build. CARGO_TARGET_DIR points cargo at the top-level mount;
# fingerprints are keyed on the SOURCE path (/work, identical across
# worktrees) not the target path, so the per-worktree-volume collision
# rationale above is unchanged.
exec docker run --rm \
  --platform linux/arm64 \
  --entrypoint "" \
  -v "$WORKTREE:/work" \
  -v "$TARGET_VOLUME:/target" \
  -e "CARGO_HOME=$CARGO_HOME_IN_CONTAINER" \
  -e "CARGO_TARGET_DIR=/target" \
  -v "$CARGO_HOME_VOLUME:$CARGO_HOME_IN_CONTAINER" \
  -w /work \
  "$IMAGE" "$@"
