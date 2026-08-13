#!/usr/bin/env bash
# Unit tests for scripts/dev.sh, the only path this repo has to a Rust
# toolchain (see docker/frankenrust-dev.Dockerfile and issue #5).
#
# Everything below runs against a stub `docker` placed first on PATH, so there
# is no daemon, no image, no network and no wall-clock cost: what is under test
# is the naming and lifecycle logic, i.e. which image tag and which target/
# volume a given worktree ends up using. That logic is invisible from a normal
# gate run — a wrong volume name produces a *green* gate, not a red one — which
# is exactly why it needs pinning here.
#
# Five of these cases exist because reviewers, not tests, caught the bug:
#
#   - "distinct worktrees get distinct target volumes" — a shared volume let
#     one worktree's gate build and run another worktree's code and report it
#     green (both reviewers on #5 reproduced it).
#   - "editing the Dockerfile changes the image tag" — rebuilding only when a
#     constant tag was absent froze the image at whatever was built first on
#     the host, so no Dockerfile fix could ever reach a gate run.
#   - "the cargo volume is mounted AT $CARGO_HOME" — sharing only registry/
#     shared the cache without cargo's lock over it, so concurrent gates raced
#     in registry/src and one of them failed to unpack a crate.
#   - "an unremovable superseded volume is not a failure" — invalidating the
#     target cache by removing the volume in place failed whenever a parallel
#     gate still held it, failing the innocent worktree's build.
#   - "an unlabelled volume under our own name is reclaimed" — a volume that
#     reached the daemon without dev.sh's labels (a lost create/run race, or a
#     pre-fix dev.sh) was invisible to the label-filtered reclaim scan and got
#     reused forever instead of collected; a reviewer on #5 found a live
#     instance of exactly this on the host running the loop.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd -P)"
# -P: dev.sh labels volumes with `pwd -P`, and on macOS mktemp hands back a
# /var/... path that is a symlink into /private/var.
TMP="$(cd "$(mktemp -d)" && pwd -P)"
trap 'rm -rf "$TMP"' EXIT

PASSED=0
FAILED=0

# Quiet when green, like the gate's other steps; DEV_ENV_VERBOSE=1 for the
# per-assertion trace. A failure prints the assertion and, at the end, every
# line dev.sh wrote to stderr along the way.
V="${DEV_ENV_VERBOSE:-}"
section() { [ -n "$V" ] && echo "$1"; return 0; }
ok()   { [ -n "$V" ] && echo "    ok   $1"; PASSED=$((PASSED + 1)); return 0; }
bad()  { echo "    FAIL $1"; FAILED=$((FAILED + 1)); }
assert_eq() { [ "$2" = "$3" ] && ok "$1" || bad "$1: expected '$2', got '$3'"; }
assert_ne() { [ "$2" != "$3" ] && ok "$1" || bad "$1: both values are '$2'"; }
assert_nonempty() { [ -n "$2" ] && ok "$1" || bad "$1: value is empty"; }
assert_log_has() {
  grep -qF "$2" "$DOCKER_LOG" && ok "$1" || bad "$1: no '$2' in docker log"
}
assert_log_lacks() {
  grep -qF "$2" "$DOCKER_LOG" && bad "$1: unexpected '$2' in docker log" || ok "$1"
}
assert_stderr_has() {
  grep -qF "$2" "$DEV_STDERR" && ok "$1" || bad "$1: no '$2' on stderr"
}

# --- stub docker -------------------------------------------------------------
# STUB_IMAGES holds one "repo:tag" per line (what `build -t` and `images
# --format {{.Repository}}:{{.Tag}}` both traffic in, so no template parsing
# is needed for that one). STUB_VOLUMES holds one volume name per line.
# STUB_VOLUME_LABELS holds "name value" pairs for the one label dev.sh ever
# reads back (com.frankenrust.image) — a real `docker volume inspect --format
# '{{index .Labels "..."}}'` prints exactly that value, or an empty line if
# the volume has no such label, and this stub does the same. That empty-line
# behaviour is also what makes it double as the WORKTREE_LABEL read: dev.sh
# checks that one for non-emptiness only, and a volume this stub never saw a
# `volume create --label` for reads back empty regardless of which label was
# asked for — the same as a genuinely unlabelled real volume would.
#
# `volume ls` is not filtered live — dev.sh issues it with two different
# `--filter`s (by label, and by name), and this stub answers each from its own
# pre-seeded file (STUB_VOLUME_LS / STUB_VOLUME_LS_BY_NAME) rather than
# deriving either from STUB_VOLUMES, so a test can set up exactly the daemon
# state a given `--filter` should see without reimplementing Docker's filter
# semantics.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/docker" <<'STUB'
#!/usr/bin/env bash
# Records every invocation and answers from files the test seeds.
printf '%s\n' "$*" >> "$DOCKER_LOG"
case "${1:-}" in
  info)
    exit "${STUB_INFO_RC:-0}"
    ;;
  image)  # image inspect <tag>
    grep -qxF "${3:-}" "$STUB_IMAGES"
    ;;
  images) # images --format {{.Repository}}:{{.Tag}} <repo>
    shift
    repo=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --format) shift 2 ;;
        *) repo="$1"; shift ;;
      esac
    done
    while IFS= read -r img; do
      case "$img" in "$repo:"*) printf '%s\n' "$img" ;; esac
    done < "$STUB_IMAGES"
    ;;
  rmi)
    [ "${STUB_RMI_RC:-0}" = 0 ] || exit "$STUB_RMI_RC"
    grep -qxF "${2:-}" "$STUB_IMAGES" || exit 1
    grep -vxF "${2:-}" "$STUB_IMAGES" > "$STUB_IMAGES.tmp"; mv "$STUB_IMAGES.tmp" "$STUB_IMAGES"
    ;;
  build)
    [ "${STUB_BUILD_RC:-0}" = 0 ] || exit "$STUB_BUILD_RC"
    while [ $# -gt 1 ]; do
      [ "$1" = "-t" ] && printf '%s\n' "$2" >> "$STUB_IMAGES"
      shift
    done
    ;;
  volume)
    case "${2:-}" in
      inspect)
        # Parsed properly rather than positionally (a previous version read
        # argv[3] as the volume name unconditionally): real `docker volume
        # inspect` takes [OPTIONS] before VOLUME, and dev.sh's own label
        # check calls it that way (`-f FMT NAME`, the idiomatic form) while
        # the pre-existing calls in this stub use `NAME --format FMT`. Both
        # must resolve to the same volume, or whichever call style is not
        # literally argv[3] silently inspects the wrong thing.
        shift 2
        vol="" fmt=""
        while [ $# -gt 0 ]; do
          case "$1" in
            -f|--format) fmt="$2"; shift 2 ;;
            *) vol="$1"; shift ;;
          esac
        done
        grep -qxF "$vol" "$STUB_VOLUMES" || exit 1
        if [ -n "$fmt" ]; then
          grep "^$vol " "$STUB_VOLUME_LABELS" 2>/dev/null | tail -1 | cut -d' ' -f2-
        fi
        ;;
      create)
        [ "${STUB_VOLUME_CREATE_RC:-0}" = 0 ] || exit "$STUB_VOLUME_CREATE_RC"
        vol="" image_label=""
        shift 2
        while [ $# -gt 0 ]; do
          case "$1" in
            --label)
              case "$2" in com.frankenrust.image=*) image_label="${2#com.frankenrust.image=}" ;; esac
              shift 2 ;;
            *) vol="$1"; shift ;;
          esac
        done
        printf '%s\n' "$vol" >> "$STUB_VOLUMES"
        printf '%s %s\n' "$vol" "$image_label" >> "$STUB_VOLUME_LABELS"
        ;;
      rm)
        [ "${STUB_VOLUME_RM_RC:-0}" = 0 ] || exit "$STUB_VOLUME_RM_RC"
        vol="${3:-}"
        # `grep -v` exits 1 when it removes every line (i.e. the file held
        # only $vol), not just on a real error -- that is "no output", not
        # "failed". A preceding version of this stub chained the mv with
        # `&&`, so removing the sole remaining volume silently kept the file
        # (and hence the volume) exactly as it was: undetectable by any test
        # that only checks the log for the `volume rm` invocation rather than
        # the state it was supposed to produce. `|| true` makes an
        # empty-output grep still commit.
        grep -vxF "$vol" "$STUB_VOLUMES" > "$STUB_VOLUMES.tmp" 2>/dev/null || true
        mv "$STUB_VOLUMES.tmp" "$STUB_VOLUMES"
        grep -v "^$vol " "$STUB_VOLUME_LABELS" > "$STUB_VOLUME_LABELS.tmp" 2>/dev/null || true
        mv "$STUB_VOLUME_LABELS.tmp" "$STUB_VOLUME_LABELS"
        ;;
      ls)
        shift 2
        filter=""
        while [ $# -gt 0 ]; do
          case "$1" in --filter) filter="$2"; shift 2 ;; *) shift ;; esac
        done
        case "$filter" in
          name=*)  cat "$STUB_VOLUME_LS_BY_NAME" 2>/dev/null ;;
          *)       cat "$STUB_VOLUME_LS" ;;
        esac
        ;;
      *)       : ;;
    esac
    ;;
  run)
    exit "${STUB_RUN_RC:-0}"
    ;;
esac
STUB
chmod +x "$TMP/bin/docker"

export DOCKER_LOG="$TMP/docker.log"
export STUB_IMAGES="$TMP/images"
export STUB_VOLUMES="$TMP/volumes"
export STUB_VOLUME_LABELS="$TMP/volume-labels"
export STUB_VOLUME_LS="$TMP/volume-ls"
export STUB_VOLUME_LS_BY_NAME="$TMP/volume-ls-by-name"

reset() {
  : > "$DOCKER_LOG"; : > "$STUB_IMAGES"; : > "$STUB_VOLUMES"
  : > "$STUB_VOLUME_LABELS"; : > "$STUB_VOLUME_LS"; : > "$STUB_VOLUME_LS_BY_NAME"
  unset STUB_INFO_RC STUB_BUILD_RC STUB_RUN_RC STUB_RMI_RC STUB_VOLUME_RM_RC
  unset STUB_VOLUME_CREATE_RC
}

# Two throwaway worktrees holding the real dev.sh and the real Dockerfile.
for w in a b; do
  mkdir -p "$TMP/$w/scripts" "$TMP/$w/docker"
  cp "$REPO/scripts/dev.sh" "$TMP/$w/scripts/dev.sh"
  cp "$REPO/docker/frankenrust-dev.Dockerfile" "$TMP/$w/docker/"
done

DEV_STDERR="$TMP/dev.stderr"
: > "$DEV_STDERR"
dev() { # dev <worktree> [args...] -> exit status of dev.sh
  local wt="$1"; shift
  PATH="$TMP/bin:$PATH" bash "$TMP/$wt/scripts/dev.sh" "$@" 2>> "$DEV_STDERR"
}
# Deliberately loose: these must also match a *constant* name, so that a
# regression to one reads as "both worktrees got the same volume" rather than
# as "no volume was found at all".
target_volume() { grep -o 'frankenrust-dev-target[-0-9a-z]*' "$DOCKER_LOG" | tail -1; }
image_tag()     { grep -o 'frankenrust-dev:[0-9a-z]\{1,\}' "$DOCKER_LOG" | tail -1; }

# --- target/ volume isolation ------------------------------------------------
section "--- dev.sh: target/ volume is per worktree"
reset; dev a true; vol_a="$(target_volume)"
reset; dev b true; vol_b="$(target_volume)"
assert_nonempty "worktree a mounts a target volume" "$vol_a"
assert_ne "distinct worktrees get distinct target volumes" "$vol_a" "$vol_b"
assert_log_has "the volume is mounted at /work/target" "$vol_b:/work/target"

reset; dev a true; vol_a2="$(target_volume)"
assert_eq "the same worktree reuses its volume across runs" "$vol_a" "$vol_a2"

reset; dev a true
assert_log_has "the volume is labelled with its worktree" \
  "volume create --label com.frankenrust.worktree=$TMP/a"

# --- image tag tracks the Dockerfile -----------------------------------------
section "--- dev.sh: image tag tracks the Dockerfile"
reset; dev a true; tag_a="$(image_tag)"
reset; dev b true; tag_b="$(image_tag)"
assert_nonempty "an image tag is derived" "$tag_a"
assert_eq "identical Dockerfiles share one image" "$tag_a" "$tag_b"

printf '\n# a change that must reach the gate\n' >> "$TMP/a/docker/frankenrust-dev.Dockerfile"
reset; dev a true; tag_a2="$(image_tag)"
assert_ne "editing the Dockerfile changes the image tag" "$tag_a" "$tag_a2"
assert_log_has "the edited Dockerfile is rebuilt" "build --platform linux/arm64 -t $tag_a2"

# An already-built tag must NOT be rebuilt, or every gate step pays for it.
printf '%s\n' "$tag_a2" > "$STUB_IMAGES"
: > "$DOCKER_LOG"
dev a true
assert_log_lacks "an unchanged Dockerfile is not rebuilt" "build --platform"

# --- superseded image tags are pruned ------------------------------------------
# Nothing else ever untags an image once its Dockerfile hash is superseded; on
# a long-running host that is one multi-hundred-MB layer per edit, forever.
section "--- dev.sh: a superseded image tag is removed after a rebuild"
reset; dev a true; itag1="$(image_tag)"
printf '\n# force a new tag\n' >> "$TMP/a/docker/frankenrust-dev.Dockerfile"
: > "$DOCKER_LOG"
dev a true; itag2="$(image_tag)"
assert_ne "the Dockerfile edit changed the tag" "$itag1" "$itag2"
assert_log_has "the superseded tag is removed" "rmi $itag1"

# --- target/ volume tracks the image it was built against ---------------------
# target/ holds cargo's build cache, which is only valid for the libphp headers
# baked into the image it was built against — and that dependency is invisible
# to cargo, since the image lives outside the bind-mounted source tree cargo
# watches. A target/ volume left over from a different image must never be
# relinked against the new one.
#
# The invariant is enforced by the volume's NAME, so these cases assert on the
# name. An earlier version enforced it by removing the wrongly-labelled volume
# in place, and these two cases asserted on that `volume rm` — see the
# "survives a peer" section below for why that mechanism was replaced. Asserting
# on the name is also strictly stronger: a `volume rm` assertion is satisfied by
# an *attempt*, so it stayed green in exactly the case that broke (the removal
# failing and the stale cache surviving), whereas a name that differs cannot be
# reused no matter what the daemon does.
section "--- dev.sh: target/ volume tracks the image it was built against"
reset; dev a true; vol1="$(target_volume)"
: > "$DOCKER_LOG"
dev a true
assert_log_lacks "reusing the same image does not discard the cache" "volume rm $vol1"
assert_eq "the volume name is unchanged when the image is unchanged" "$vol1" "$(target_volume)"

printf '\n# a change that must invalidate the target cache\n' >> "$TMP/a/docker/frankenrust-dev.Dockerfile"
: > "$DOCKER_LOG"
dev a true; vol2="$(target_volume)"
assert_ne "an image change changes the target volume name" "$vol1" "$vol2"
assert_log_has "the new image's volume is the one mounted" "$vol2:/work/target"
assert_log_lacks "the old image's cache is not mounted" "$vol1:/work/target"

# A volume from before the image was part of the name cannot collide with one
# that has it, so it is never mounted -- but it is still ours, and still a
# multi-GB leak, so it must be reclaimed rather than merely ignored.
reset
printf '%s\n' "$vol1" > "$STUB_VOLUMES"
printf '%s %s\n' "$vol1" "$TMP/a" > "$STUB_VOLUME_LS"
dev a true
assert_ne "an old-scheme volume is not reused" "$vol1" "$(target_volume)"
assert_log_has "an old-scheme volume is reclaimed" "volume rm $vol1"

# --- a superseded volume survives a peer that still holds it ------------------
# Fourth case caught by a reviewer rather than a test, and the one that replaced
# the mechanism above. `docker volume rm` fails whenever any container still
# references the volume -- including one that has merely exited without being
# reaped -- and fails with "no such volume" if a peer removed it first:
#
#   Error response from daemon: remove V: volume is in use - [fc13f294f284]
#   Error response from daemon: get V: no such volume
#
# The old code routed that to `exit 1`, which scripts/gate.sh:20 turns into
# `FAIL build` for a worktree whose code is fine. The trigger is ordinary: the
# loop runs FR_PARALLEL=3 gates at once, so any merge editing the Dockerfile
# lands while peers hold the old volume. Deriving the name instead means a
# failed reclaim is a leak to warn about, not a gate failure -- so this asserts
# the run SUCCEEDS with every `volume rm` failing.
section "--- dev.sh: a superseded volume held by a peer does not fail the run"
reset; dev a true; held="$(target_volume)"
printf '\n# an image change landing while a peer holds the old volume\n' \
  >> "$TMP/a/docker/frankenrust-dev.Dockerfile"
printf '%s %s\n' "$held" "$TMP/a" > "$STUB_VOLUME_LS"
: > "$DOCKER_LOG"
STUB_VOLUME_RM_RC=1 dev a true; rc=$?
assert_eq "an unremovable superseded volume is not a failure" 0 "$rc"
assert_log_has "the reclaim is still attempted" "volume rm $held"
assert_stderr_has "the leak is warned about, not swallowed" \
  "WARNING: could not reclaim $held"
assert_log_has "the run proceeds on a fresh volume" "$(target_volume):/work/target"
assert_ne "and that volume is not the one it could not remove" "$held" "$(target_volume)"

# A peer worktree's volume is not ours to reclaim: a sibling on a different
# image is mid-run with another branch checked out, not stale.
reset; dev a true
printf '%s %s\n' "frankenrust-dev-target-0123456789ab-fedcba987654" "$TMP/b" \
  > "$STUB_VOLUME_LS"
: > "$DOCKER_LOG"
dev a true
assert_log_lacks "a live peer worktree's volume is left alone" \
  "volume rm frankenrust-dev-target-0123456789ab-fedcba987654"

# --- the shared cargo cache carries cargo's own lock --------------------------
# The cargo cache is shared across worktrees on purpose, but cargo's
# package-cache lock -- the mutex that serialises unpacking a .crate into
# registry/src -- lives at $CARGO_HOME/.package-cache, one level ABOVE
# registry/. Mounting the volume at $CARGO_HOME/registry therefore shares the
# mutable state without the mutex, and concurrent gates (FR_PARALLEL=3 is the
# default) corrupt each other's unpack: "failed to unpack package `socket2
# v0.6.5` ... .cargo-ok: File exists". Reproduced, then fixed, on #5.
#
# Third case caught by a reviewer rather than a test, hence this section. The
# invariant is an equality: the volume's mount point must BE the container's
# CARGO_HOME, not a directory inside it.
section "--- dev.sh: the shared cargo cache includes cargo's own lock"
reset; dev a true
run_line="$(grep '^run --rm' "$DOCKER_LOG" | tail -1)"
cargo_home="$(printf '%s\n' "$run_line" | grep -o 'CARGO_HOME=[^ ]\{1,\}' | cut -d= -f2)"
cargo_mount="$(printf '%s\n' "$run_line" | grep -o 'frankenrust-dev-cargo[^ ]\{1,\}' | cut -d: -f2-)"
assert_nonempty "the container is given an explicit CARGO_HOME" "$cargo_home"
assert_nonempty "a shared cargo volume is mounted" "$cargo_mount"
assert_eq "the cargo volume is mounted AT \$CARGO_HOME, not inside it" \
  "$cargo_home" "$cargo_mount"
# Mounting over the image's own /usr/local/cargo would satisfy the equality
# above and still be wrong: Docker seeds a named volume from the image
# directory it covers exactly once, so the volume would pin that toolchain and
# a Rust version bump in the Dockerfile could never reach a gate run.
assert_ne "\$CARGO_HOME is not the image's toolchain directory" \
  "/usr/local/cargo" "$cargo_home"
# The registry-only volume the first version of this script used is now
# unreferenced, and carries none of the labels the reclaim loop filters on.
assert_log_has "the superseded registry-only volume is reclaimed" \
  "volume rm frankenrust-dev-cargo-registry"

# --- fail closed --------------------------------------------------------------
section "--- dev.sh: fails closed"
reset; STUB_INFO_RC=1 dev a true; rc=$?
assert_eq "no docker daemon is a failure" 1 "$rc"
assert_log_lacks "no daemon means nothing ran" "run --rm"

reset; STUB_BUILD_RC=1 dev a true; rc=$?
assert_eq "a failed image build is a failure" 1 "$rc"
assert_log_lacks "a failed build means nothing ran" "run --rm"

reset; dev a; rc=$?
assert_eq "no command is a usage error" 2 "$rc"

reset; STUB_RUN_RC=7 dev a false; rc=$?
assert_eq "the command's exit status is propagated" 7 "$rc"

# `docker run -v` auto-creates a missing named volume, so a failed `volume
# create` does NOT surface as a failed run: it surfaces as a correctly-named
# but unlabelled volume, which the reclaim loop filters on and so would never
# collect. That is a permanent multi-GB leak that reports itself as success.
reset; STUB_VOLUME_CREATE_RC=1 dev a true; rc=$?
assert_eq "a volume that cannot be created is a failure" 1 "$rc"
assert_log_lacks "an uncreatable volume means nothing ran" "run --rm"

# --- stale volume reclaim -----------------------------------------------------
# Per-worktree volumes would otherwise accumulate one multi-GB target/ per
# issue over a long run. Only our own labelled volumes, only once their
# worktree is gone.
section "--- dev.sh: reclaims volumes whose worktree is gone"
reset
cat > "$STUB_VOLUME_LS" <<EOF
frankenrust-dev-target-deadbeefcafe $TMP/worktree-that-was-removed
frankenrust-dev-target-0123456789ab $TMP/b
EOF
dev a true
assert_log_has "a volume whose worktree vanished is removed" \
  "volume rm frankenrust-dev-target-deadbeefcafe"
assert_log_lacks "a live worktree's volume is left alone" \
  "volume rm frankenrust-dev-target-0123456789ab"
assert_log_has "only our own volumes are considered" \
  "volume ls --filter label=com.frankenrust.worktree"

# A `docker volume rm` failure here (e.g. the volume is still attached to a
# container orphaned by a killed gate run) must not vanish silently -- that
# is exactly how the leak this loop exists to prevent goes unnoticed.
reset
cat > "$STUB_VOLUME_LS" <<EOF
frankenrust-dev-target-deadbeefcafe $TMP/worktree-that-was-removed
EOF
STUB_VOLUME_RM_RC=1 dev a true
assert_log_has "a failed reclaim is still attempted" \
  "volume rm frankenrust-dev-target-deadbeefcafe"
assert_stderr_has "a failed reclaim is warned about, not swallowed" \
  "could not reclaim frankenrust-dev-target-deadbeefcafe"

# --- unlabelled-volume reclaim -------------------------------------------------
# `docker volume create NAME` issued a second time for a NAME that already
# exists is not an error -- the daemon silently hands back the existing
# volume instead of applying the new call's options -- so a volume can reach
# the daemon under one of dev.sh's names without ever going through the
# labelled `create` this script issues: two invocations racing on the same
# worktree+image, or a volume left by a version of dev.sh from before this
# labelling scheme existed. The label-filtered reclaim scan tested above can
# never find it, by construction -- it filters on a label such a volume does
# not have -- and dev.sh's own `docker volume inspect "$TARGET_VOLUME" ||
# create` short-circuits on the first branch and reuses it exactly as if this
# script had created it. A reviewer on #5 found a live instance of this: an
# orphaned volume, `docker volume inspect` reporting `Labels: map[]`, whose
# name traced to a superseded image hash.
section "--- dev.sh: an unlabelled volume under our own name is reclaimed and relabelled"
reset; dev a true; vol1="$(target_volume)"
# The volume already exists under this exact name -- `docker volume inspect`
# will find it -- but carries none of dev.sh's labels, because nothing routed
# it through `volume create --label ...`. That is precisely what `docker
# volume ls --filter name=...` (unlike the label-filtered scan) would still
# return it for.
printf '%s\n' "$vol1" > "$STUB_VOLUMES"
: > "$STUB_VOLUME_LABELS"
printf '%s\n' "$vol1" > "$STUB_VOLUME_LS_BY_NAME"
: > "$DOCKER_LOG"
dev a true
assert_log_has "the unlabelled volume is discarded, not reused" "volume rm $vol1"
assert_log_has "a properly labelled volume is created under the same name" \
  "volume create --label com.frankenrust.worktree=$TMP/a"
assert_log_has "the recreated volume is still the one mounted" "$vol1:/work/target"

# A superseded (different-image) sibling can be unlabelled for the same
# reason. It must be reclaimed too, but a sibling that IS labelled -- in
# particular $TARGET_VOLUME itself, once the case above has relabelled it --
# is not this pass's business; the label-filtered scan already owns it.
section "--- dev.sh: an unlabelled superseded sibling under our own prefix is reclaimed"
reset; dev a true; vol1="$(target_volume)"
sibling="${vol1%-*}-000000000000"
printf '%s\n' "$sibling" >> "$STUB_VOLUMES"
printf '%s\n%s\n' "$vol1" "$sibling" > "$STUB_VOLUME_LS_BY_NAME"
: > "$DOCKER_LOG"
dev a true
assert_log_has "the unlabelled sibling is reclaimed" "volume rm $sibling"
assert_log_lacks "the already-labelled current volume is left alone by this pass" \
  "volume rm $vol1"
assert_eq "the current volume's name is unchanged" "$vol1" "$(target_volume)"

if [ "$FAILED" -eq 0 ]; then
  echo "dev-env: $PASSED passed"
  exit 0
fi
echo "--- what dev.sh printed on stderr:"
sed 's/^/    /' "$DEV_STDERR"
echo "dev-env: $FAILED failed, $PASSED passed"
exit 1
