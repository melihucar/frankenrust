#!/usr/bin/env bash
# Verification gate. The orchestrator runs this in each agent's worktree and
# discards the work unless it exits 0. Agents are told to run it themselves too.
#
#   ./scripts/gate.sh [profile]
#
# Profiles:
#   bootstrap   build only — for early tasks that predate the test suite
#   default     build + clippy + fmt + unit tests + conformance vs FrankenPHP
#   bench       default, plus the benchmark harness must run end to end
set -uo pipefail

PROFILE="${1:-default}"
cd "$(dirname "$0")/.." || exit 1
FAILED=()

step() {
  local name="$1"; shift
  echo "--- gate: $name"
  if "$@"; then
    echo "    PASS $name"
  else
    echo "    FAIL $name"
    FAILED+=("$name")
  fi
}

# Fail closed: an agent that deletes the test suite must not get a green gate.
step "test-suite-intact" bash -c '
  [ -d tests ] || { echo "tests/ is missing"; exit 1; }
  n=$(grep -rl "#\[test\]\|#\[tokio::test\]" tests src 2>/dev/null | wc -l | tr -d " ")
  min=$(cat .gate/min-test-files 2>/dev/null || echo 0)
  [ "$n" -ge "$min" ] || { echo "test files: $n < required $min"; exit 1; }
  ! grep -rn "#\[ignore\]" tests src 2>/dev/null | grep -v "GATE-OK" || {
    echo "found #[ignore] without a GATE-OK justification"; exit 1; }
'

# The orchestrator can now merge changes to itself and restart into them, so a
# broken loop.py would end the run with no human around to restart it. This is
# the check that makes self-modification survivable: it runs in every profile,
# and it is why a syntax error, an import-time crash, or a role with no prompt
# file can never reach main.
step "orchestrator-runnable" python3 scripts/check_orchestrator.py

# docs/ cites vendor/frankenphp/<file>:<line> as its evidence for every claim
# about upstream behaviour. Those citations rot silently: a vendor bump shifts
# line numbers, a doc edit outpaces the code it describes, and nothing short
# of a reviewer re-reading the cited lines catches it. This runs both the
# checker's own negative cases and the real check against docs/, so the
# negative cases are gate-enforced rather than rotting unrun. No Rust
# toolchain, so it runs in every profile, including bootstrap, since docs rot
# before Cargo.toml exists too.
step "doc-citations" bash -c 'python3 scripts/check_doc_citations.py --selftest \
  && python3 scripts/check_doc_citations.py'

# scripts/dev.sh is the only route to a Rust toolchain, so a bug in which image
# tag or which target/ volume it picks is a bug in build, fmt, clippy and test
# at once — and it fails *green*: a worktree that reuses another worktree's
# artifacts gets a passing gate for code that was never compiled. Nothing else
# here can see that, so it is pinned directly. Pure shell against a stub
# docker: no daemon, no image, runs in every profile.
step "dev-env" bash tests/dev-env/dev-sh.test.sh

# Early backlog tasks (the PHP base image, the conformance harness) legitimately
# produce no Rust. Tolerate a missing workspace ONLY in bootstrap; in any other
# profile a vanished Cargo.toml means someone deleted the project.
if [ ! -f Cargo.toml ]; then
  if [ "$PROFILE" = "bootstrap" ]; then
    echo "--- gate: build SKIPPED (no Cargo.toml yet — pre-workspace task)"
  else
    echo "--- gate: build"; echo "    FAIL build (Cargo.toml missing in profile $PROFILE)"
    FAILED+=("build")
  fi
else
  # Routed through scripts/dev.sh: the host has no Rust toolchain and its PHP
  # is neither ZTS nor built with the embed SAPI this port links against, so
  # `cargo build` cannot work outside the frankenrust-dev container. dev.sh
  # itself fails loudly (not skips) if Docker is unavailable or the image
  # cannot be built, and step() below turns that failure into a gate FAIL.
  step "build" bash scripts/dev.sh cargo build --workspace --all-targets
fi

if [ "$PROFILE" != "bootstrap" ]; then
  step "fmt"     bash scripts/dev.sh cargo fmt --all -- --check
  step "clippy"  bash scripts/dev.sh cargo clippy --workspace --all-targets -- -D warnings
  step "test"    bash scripts/dev.sh cargo test --workspace
  step "conformance" bash tests/conformance/run.sh
fi

if [ "$PROFILE" = "bench" ]; then
  step "bench-smoke" bash bench/harness/run.sh --smoke
fi

step "bench-report-selftest" python3 bench/harness/report.py --selftest

echo
if [ ${#FAILED[@]} -eq 0 ]; then
  echo "GATE PASS ($PROFILE)"
  exit 0
fi
echo "GATE FAIL ($PROFILE): ${FAILED[*]}"
exit 1
