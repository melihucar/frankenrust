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
  step "build" cargo build --workspace --all-targets
fi

if [ "$PROFILE" != "bootstrap" ]; then
  step "fmt"     cargo fmt --all -- --check
  step "clippy"  cargo clippy --workspace --all-targets -- -D warnings
  step "test"    cargo test --workspace
  step "conformance" bash tests/conformance/run.sh
fi

if [ "$PROFILE" = "bench" ]; then
  step "bench-smoke" bash bench/harness/run.sh --smoke
fi

echo
if [ ${#FAILED[@]} -eq 0 ]; then
  echo "GATE PASS ($PROFILE)"
  exit 0
fi
echo "GATE FAIL ($PROFILE): ${FAILED[*]}"
exit 1
